import AppKit
import SwiftTerm
import SwiftUI

/// Terminals outlive their tab: switching to Logs and back finds the same
/// shell. A session that ended (its shell exited, its container or machine
/// stopped) is replaced by a new one when shown again; past `limit`, the one
/// unused longest is closed.
@MainActor
final class TerminalStore {
    static let shared = TerminalStore()

    struct Session {
        let view: TerminalView
        let ended: () -> Bool
        let close: () -> Void
    }

    private var sessions: [String: Session] = [:]
    /// Least recently shown first.
    private var order: [String] = []
    private let limit = 8

    func view(_ key: String, make: () -> Session) -> TerminalView {
        if let session = sessions[key], session.ended() {
            session.close()
            sessions[key] = nil
        }
        let session = sessions[key] ?? make()
        sessions[key] = session
        order.removeAll { $0 == key }
        order.append(key)
        while order.count > limit {
            let oldest = order.removeFirst()
            sessions.removeValue(forKey: oldest)?.close()
        }
        return session.view
    }
}

/// Hosts a stored terminal: removing the host from the window (a tab switch)
/// leaves the session running.
struct StoredTerminal: NSViewRepresentable {
    let key: String
    let make: @MainActor () -> TerminalStore.Session

    func makeNSView(context: Context) -> NSView {
        let host = NSView()
        let terminal = TerminalStore.shared.view(key, make: make)
        terminal.removeFromSuperview()
        terminal.frame = host.bounds
        terminal.autoresizingMask = [.width, .height]
        host.addSubview(terminal)
        DispatchQueue.main.async { terminal.window?.makeFirstResponder(terminal) }
        return host
    }

    func updateNSView(_ host: NSView, context: Context) {}

    static func dismantleNSView(_ host: NSView, coordinator: ()) {
        host.subviews.forEach { $0.removeFromSuperview() }
    }
}

/// Interactive shell in a container through Docker's exec API: a hijacked,
/// upgraded connection carrying raw TTY bytes in both directions.
struct ContainerTerminal: View {
    let containerID: String

    var body: some View {
        StoredTerminal(key: "exec-\(containerID)") {
            let session = ExecSession(containerID: containerID)
            let view = TerminalView(frame: .zero)
            view.terminalDelegate = session
            configure(view)
            session.attach(view)
            return .init(view: view, ended: { session.ended }, close: { session.close() })
        }
        .id(containerID)
    }
}

private func configure(_ view: TerminalView) {
    view.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
    view.optionAsMetaKey = true
}

/// Its fields are set by the thread that starts the exec and read by the
/// main thread and the write queue: all behind `lock`.
final class ExecSession: NSObject, TerminalViewDelegate, @unchecked Sendable {
    let containerID: String
    private let lock = NSLock()
    private var socket: UnixSocket?
    private var execID: String?
    private var closed = false
    private var finished = false
    private weak var view: TerminalView?
    private let writeQueue = DispatchQueue(label: "berth.exec.write")
    private var size = (cols: 80, rows: 24)

    /// The shell is gone (or never started): show a new one next time.
    var ended: Bool { lock.withLock { finished } }

    init(containerID: String) { self.containerID = containerID }

    func attach(_ view: TerminalView) {
        self.view = view
        let id = containerID
        Task.detached { [weak self] in
            do {
                let created = try await UnixHTTP.request(
                    Docker.socket, "POST", "/containers/\(id)/exec",
                    json: [
                        "AttachStdin": true, "AttachStdout": true, "AttachStderr": true, "Tty": true,
                        "Env": ["TERM=xterm-256color"],
                        "Cmd": ["sh", "-c", "if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh -l; fi"],
                    ]
                )
                guard let execID = (try JSONSerialization.jsonObject(with: created.body) as? [String: Any])?["Id"] as? String
                else { throw SocketError.message("exec create returned no id") }
                let (sock, initial) = try await offPool {
                    try UnixHTTP.upgrade(Docker.socket, "/exec/\(execID)/start", json: ["Detach": false, "Tty": true])
                }
                guard let self else {
                    sock.shutdownBoth()
                    return
                }
                self.started(sock: sock, execID: execID, initial: initial)
            } catch {
                self?.finish()
                self?.show("\r\n\u{1b}[31m\(error.localizedDescription)\u{1b}[0m\r\n")
            }
        }
    }

    private func started(sock: UnixSocket, execID: String, initial: Data) {
        // Closed while the exec was starting: nothing else would shut it.
        let open = lock.withLock {
            if !closed {
                socket = sock
                self.execID = execID
            }
            return !closed
        }
        guard open else {
            sock.shutdownBoth()
            return
        }
        if !initial.isEmpty { feed(initial) }
        resize()
        Thread.detachNewThread { [weak self] in
            while let chunk = try? sock.read() { self?.feed(chunk) }
            self?.finish()
            self?.show("\r\n\u{1b}[2m[session ended]\u{1b}[0m\r\n")
        }
    }

    private func finish() { lock.withLock { finished = true } }

    private func feed(_ data: Data) {
        let bytes = [UInt8](data)
        DispatchQueue.main.async { self.view?.feed(byteArray: bytes[...]) }
    }

    private func show(_ text: String) {
        DispatchQueue.main.async { self.view?.feed(text: text) }
    }

    private func resize() {
        guard let (execID, (cols, rows)) = lock.withLock({ self.execID.map { ($0, size) } }) else { return }
        Task.detached {
            _ = try? await UnixHTTP.request(Docker.socket, "POST", "/exec/\(execID)/resize?h=\(rows)&w=\(cols)")
        }
    }

    func close() {
        let sock = lock.withLock {
            closed = true
            finished = true
            return socket
        }
        sock?.shutdownBoth()
    }

    // MARK: TerminalViewDelegate

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        let bytes = Data(data)
        writeQueue.async { [weak self] in
            guard let sock = self?.lock.withLock({ self?.socket }) else { return }
            try? sock.write(bytes)
        }
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        lock.withLock { size = (newCols, newRows) }
        resize()
    }

    func setTerminalTitle(source: TerminalView, title: String) {}
    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    func scrolled(source: TerminalView, position: Double) {}
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
}

extension View {
    /// A little room around a terminal, in its own black, so text doesn't
    /// touch the window's edge.
    func terminalInset() -> some View {
        padding(.horizontal, 8).padding(.vertical, 6).background(Color.black)
    }
}

/// A shell via the berth CLI: the VM (`berth shell`) or a machine
/// (`berth machine shell <name>`).
struct MachineTerminal: View {
    var args: [String] = ["shell"]

    var body: some View {
        StoredTerminal(key: "cli-" + args.joined(separator: " ")) {
            let view = LocalProcessTerminalView(frame: .zero)
            let watcher = ProcessWatcher()
            view.processDelegate = watcher
            configure(view)
            var env = ProcessInfo.processInfo.environment
            env["TERM"] = "xterm-256color"
            view.startProcess(executable: BerthPaths.cli, args: args, environment: env.map { "\($0.key)=\($0.value)" })
            // The view holds its delegate weakly; the session keeps it.
            return .init(view: view, ended: { watcher.exited }, close: { _ = watcher; view.terminate() })
        }
        .id(args)
    }

    final class ProcessWatcher: NSObject, LocalProcessTerminalViewDelegate {
        var exited = false
        func sizeChanged(source: LocalProcessTerminalView, newCols: Int, newRows: Int) {}
        func setTerminalTitle(source: LocalProcessTerminalView, title: String) {}
        func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        func processTerminated(source: TerminalView, exitCode: Int32?) {
            exited = true
            source.feed(text: "\r\n\u{1b}[2m[shell exited]\u{1b}[0m\r\n")
        }
    }
}
