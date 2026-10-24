import AppKit
import SwiftTerm
import SwiftUI

/// Interactive shell in a container through Docker's exec API: a hijacked,
/// upgraded connection carrying raw TTY bytes in both directions.
struct ContainerTerminal: NSViewRepresentable {
    let containerID: String

    func makeCoordinator() -> ExecSession { ExecSession(containerID: containerID) }

    func makeNSView(context: Context) -> TerminalView {
        let view = TerminalView(frame: .zero)
        view.terminalDelegate = context.coordinator
        configure(view)
        context.coordinator.attach(view)
        return view
    }

    func updateNSView(_ view: TerminalView, context: Context) {}

    static func dismantleNSView(_ view: TerminalView, coordinator: ExecSession) { coordinator.close() }
}

private func configure(_ view: TerminalView) {
    view.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
    view.optionAsMetaKey = true
}

final class ExecSession: NSObject, TerminalViewDelegate, @unchecked Sendable {
    let containerID: String
    private var socket: UnixSocket?
    private var execID: String?
    private weak var view: TerminalView?
    private let writeQueue = DispatchQueue(label: "berth.exec.write")
    private var size = (cols: 80, rows: 24)

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
                let (sock, initial) = try UnixHTTP.upgrade(
                    Docker.socket, "/exec/\(execID)/start", json: ["Detach": false, "Tty": true]
                )
                self?.started(sock: sock, execID: execID, initial: initial)
            } catch {
                self?.show("\r\n\u{1b}[31m\(error.localizedDescription)\u{1b}[0m\r\n")
            }
        }
    }

    private func started(sock: UnixSocket, execID: String, initial: Data) {
        socket = sock
        self.execID = execID
        if !initial.isEmpty { feed(initial) }
        resize()
        Thread.detachNewThread { [weak self] in
            while let chunk = try? sock.read() { self?.feed(chunk) }
            self?.show("\r\n\u{1b}[2m[session ended]\u{1b}[0m\r\n")
        }
    }

    private func feed(_ data: Data) {
        let bytes = [UInt8](data)
        DispatchQueue.main.async { self.view?.feed(byteArray: bytes[...]) }
    }

    private func show(_ text: String) {
        DispatchQueue.main.async { self.view?.feed(text: text) }
    }

    private func resize() {
        guard let execID else { return }
        let (cols, rows) = size
        Task.detached {
            _ = try? await UnixHTTP.request(Docker.socket, "POST", "/exec/\(execID)/resize?h=\(rows)&w=\(cols)")
        }
    }

    func close() { socket?.shutdownBoth() }

    // MARK: TerminalViewDelegate

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        let bytes = Data(data)
        writeQueue.async { [weak self] in try? self?.socket?.write(bytes) }
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        size = (newCols, newRows)
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
struct MachineTerminal: NSViewRepresentable {
    var args: [String] = ["shell"]

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> LocalProcessTerminalView {
        let view = LocalProcessTerminalView(frame: .zero)
        view.processDelegate = context.coordinator
        configure(view)
        var env = ProcessInfo.processInfo.environment
        env["TERM"] = "xterm-256color"
        view.startProcess(executable: BerthPaths.cli, args: args, environment: env.map { "\($0.key)=\($0.value)" })
        return view
    }

    func updateNSView(_ view: LocalProcessTerminalView, context: Context) {}

    static func dismantleNSView(_ view: LocalProcessTerminalView, coordinator: Coordinator) { view.terminate() }

    final class Coordinator: NSObject, LocalProcessTerminalViewDelegate {
        func sizeChanged(source: LocalProcessTerminalView, newCols: Int, newRows: Int) {}
        func setTerminalTitle(source: LocalProcessTerminalView, title: String) {}
        func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        func processTerminated(source: TerminalView, exitCode: Int32?) {
            source.feed(text: "\r\n\u{1b}[2m[shell exited]\u{1b}[0m\r\n")
        }
    }
}
