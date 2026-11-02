// One function per protocol verb (docs/gui-protocol.md). Each takes the
// request's args and returns a Reply; errors thrown as GuiError become
// `{"ok":false,"error":…}`. Pointer verbs report `hit`, the element under the
// pointer before it acted, so the host's log says what was clicked.
import AppKit
import Darwin
import Foundation
import GuiCore

final class Handler {
    let sessions = SessionBook()

    func handle(_ req: Request, connection: Connection) -> Reply {
        do {
            return try dispatch(req, connection)
        } catch let e as GuiError {
            return .failure(e.message)
        } catch {
            return .failure("\(error)")
        }
    }

    private func dispatch(_ req: Request, _ conn: Connection) throws -> Reply {
        let a = req.args
        switch req.verb {
        case "hello": return hello()
        case "session.new":
            return .success(["session": .string(sessions.new(agent: a["agent"]?.string))])
        case "session.end":
            sessions.end(a["session"]?.string ?? req.session)
            return .success()
        case "session.list": return .success(sessions.listJSON())
        case "push": return try push(a, req.payloadLength, conn)
        case "open": return try open(a, session: req.session)
        case "screenshot": return try screenshot(a)
        case "click": return try click(a)
        case "type": return try type(a)
        case "key": return try key(a)
        case "scroll": return try scroll(a)
        case "drag": return try drag(a)
        case "tree": return try tree(a)
        case "wait": return try wait(a)
        case "windows": return .success(Apps.windows())
        case "focus":
            try Apps.focus(try required(a, "app"))
            return .success()
        case "quit":
            try Apps.quit(try required(a, "app"))
            return .success()
        default:
            throw GuiError("unknown verb '\(req.verb)'")
        }
    }

    private func required(_ a: JSON, _ key: String) throws -> String {
        guard let s = a[key]?.string, !s.isEmpty else { throw GuiError("missing '\(key)'") }
        return s
    }

    private func hello() -> Reply {
        let (size, scale) = Capture.screen()
        return .success([
            "os": "macos",
            "agent": .string(agentVersion),
            "screen": [.number(size.width), .number(size.height)],
            "scale": .number(scale),
            // Not in the table, but it saves the host a failed click to learn
            // that the base image was never granted.
            "permissions": ["accessibility": .bool(AX.trusted), "screen_recording": .bool(Capture.allowed)],
        ])
    }

    // MARK: push

    private func push(_ a: JSON, _ length: Int?, _ conn: Connection) throws -> Reply {
        let name = try PushPath.validateName(try required(a, "name"))
        let folder = PushPath.destination(a["dest"]?.string, home: NSHomeDirectory())
        let fm = FileManager.default
        try fm.createDirectory(atPath: folder, withIntermediateDirectories: true)
        let target = (folder as NSString).appendingPathComponent(name)
        // A re-push replaces the old copy whole; merging would leave files the
        // new build deleted.
        if fm.fileExists(atPath: target) { try fm.removeItem(atPath: target) }
        if a["tar"]?.bool ?? true {
            try untar(into: folder, conn: conn, length: length)
            guard fm.fileExists(atPath: target) else {
                throw GuiError("push: the tar stream has no top-level '\(name)'")
            }
        } else {
            guard fm.createFile(atPath: target, contents: nil),
                  let h = FileHandle(forWritingAtPath: target) else {
                throw GuiError("push: can't create \(target)")
            }
            defer { try? h.close() }
            try conn.readPayload(length: length) { try h.write(contentsOf: $0) }
        }
        return .success(["path": .string(target)])
    }

    /// Streams the payload into /usr/bin/tar. bsdtar refuses absolute paths
    /// and ".." members unless given -P, so a tar can't write outside folder.
    private func untar(into folder: String, conn: Connection, length: Int?) throws {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/tar")
        p.arguments = ["-x", "-f", "-", "-C", folder]
        let input = Pipe()
        let err = Pipe()
        p.standardInput = input
        p.standardError = err
        p.standardOutput = FileHandle.nullDevice
        try p.run()
        // Collect stderr on the side, or a chatty tar fills its pipe and stalls.
        var errText = Data()
        let errDone = DispatchSemaphore(value: 0)
        DispatchQueue.global().async {
            errText = err.fileHandleForReading.readDataToEndOfFile()
            errDone.signal()
        }
        let wfd = input.fileHandleForWriting.fileDescriptor
        var writeFailed = false
        do {
            try conn.readPayload(length: length) { chunk in
                if writeFailed { return } // tar quit; drain nothing more into it
                chunk.withUnsafeBytes { raw in
                    var off = 0
                    while off < raw.count {
                        // write(2), not FileHandle.write: EPIPE from a tar
                        // that exited early must be an error, not a crash.
                        let n = Darwin.write(wfd, raw.baseAddress! + off, raw.count - off)
                        if n > 0 { off += n } else if n < 0 && errno == EINTR { continue } else { writeFailed = true; return }
                    }
                }
            }
        } catch {
            try? input.fileHandleForWriting.close()
            p.terminate()
            p.waitUntilExit()
            throw error
        }
        try? input.fileHandleForWriting.close()
        p.waitUntilExit()
        errDone.wait()
        if p.terminationStatus != 0 {
            let msg = String(decoding: errText, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
            throw GuiError("push: tar failed: \(msg.isEmpty ? "exit \(p.terminationStatus)" : msg)")
        }
    }

    // MARK: apps

    private func open(_ a: JSON, session: String?) throws -> Reply {
        let app = try required(a, "app")
        let args = (a["args"]?.array ?? []).compactMap { $0.string }
        let wait = min(max(a["wait_s"]?.double ?? 20, 1), 300)
        let (pid, title, name) = try Apps.open(app, args: args, wait: wait)
        sessions.noteApp(name, session: session)
        let window = (title?.isEmpty == false) ? title! : name
        return .success(["pid": .int(Int(pid)), "window": .string(window)])
    }

    private func screenshot(_ a: JSON) throws -> Reply {
        let app = try a["window"]?.string.map { try Apps.find($0) }
        let shot = try Capture.shoot(app: app, format: a["format"]?.string ?? "png")
        return .success(["width": .int(shot.width), "height": .int(shot.height)], payload: shot.data)
    }

    // MARK: input

    private func point(_ a: JSON, _ xKey: String = "x", _ yKey: String = "y") -> CGPoint? {
        guard let x = a[xKey]?.double, let y = a[yKey]?.double else { return nil }
        return CGPoint(x: x, y: y)
    }

    /// The tree for label lookups: deep, because buttons sit many levels
    /// under the window (window > split group > scroll area > group > …).
    private func labelTree(_ app: String?) throws -> (AX.Tree, NSRunningApplication) {
        try AX.requireTrust()
        let target = try Apps.target(app)
        return (AX.tree(AX.app(target.processIdentifier), depth: 40), target)
    }

    /// Windows first, then the menu bar (see Node.pruned).
    private func findLabel(_ label: String, in tree: AX.Tree) throws -> Node {
        do {
            return try LabelMatch.find(label, in: tree.root.pruned(roles: ["AXMenuBar"]))
        } catch let e as GuiError where e.message.hasPrefix("no element") {
            do {
                return try LabelMatch.find(label, in: tree.root)
            } catch let e2 as GuiError where e2.message.hasPrefix("no element") && tree.truncated {
                throw GuiError("\(e2.message) (the tree was cut at \(maxTreeNodes) elements; pass \"app\" or a more specific label)")
            }
        }
    }

    private func click(_ a: JSON) throws -> Reply {
        try AX.requireTrust()
        let right = a["button"]?.string?.lowercased() == "right"
        let count = a["count"]?.int ?? 1
        if let label = a["label"]?.string {
            let (tree, _) = try labelTree(a["app"]?.string)
            let node = try findLabel(label, in: tree)
            guard let f = node.frame, !f.isEmpty else {
                // Off-screen or frameless (a collapsed menu's item): pressing
                // it through AX is the only way to reach it.
                guard !right, let el = tree.elements[node.path], AX.press(el) else {
                    throw GuiError("'\(label)' has no on-screen frame to click")
                }
                return .success(["pressed": true], hit: node.hitJSON)
            }
            let (x, y) = f.center
            Input.click(at: CGPoint(x: x, y: y), right: right, count: count)
            return .success(hit: node.hitJSON)
        }
        guard let p = point(a) else { throw GuiError("click needs x and y, or label") }
        let hit = AX.hit(at: p.x, p.y)
        Input.click(at: p, right: right, count: count)
        return .success(hit: hit)
    }

    private func type(_ a: JSON) throws -> Reply {
        try AX.requireTrust()
        guard let text = a["text"]?.string else { throw GuiError("missing 'text'") }
        Input.type(text)
        return .success()
    }

    private func key(_ a: JSON) throws -> Reply {
        try AX.requireTrust()
        Input.key(try KeyCombo.parse(try required(a, "combo")))
        return .success()
    }

    private func scroll(_ a: JSON) throws -> Reply {
        try AX.requireTrust()
        let (size, _) = Capture.screen()
        let p = point(a) ?? CGPoint(x: size.width / 2, y: size.height / 2)
        let hit = AX.hit(at: p.x, p.y)
        Input.scroll(at: p, dx: a["dx"]?.double ?? 0, dy: a["dy"]?.double ?? 0)
        return .success(hit: hit)
    }

    private func drag(_ a: JSON) throws -> Reply {
        try AX.requireTrust()
        guard let (fx, fy) = a["from"]?.point, let (tx, ty) = a["to"]?.point else {
            throw GuiError("drag needs from [x,y] and to [x,y]")
        }
        let hit = AX.hit(at: fx, fy)
        Input.drag(from: CGPoint(x: fx, y: fy), to: CGPoint(x: tx, y: ty))
        return .success(hit: hit)
    }

    // MARK: reading

    private func tree(_ a: JSON) throws -> Reply {
        try AX.requireTrust()
        let target = try Apps.target(a["app"]?.string)
        let depth = min(max(a["depth"]?.int ?? 12, 0), 60)
        let t = AX.tree(AX.app(target.processIdentifier), depth: depth)
        return .success(["tree": t.root.json, "truncated": .bool(t.truncated)])
    }

    private func wait(_ a: JSON) throws -> Reply {
        let timeout = min(max(a["timeout_s"]?.double ?? 20, 0), 600)
        let start = Date()
        let deadline = start.addingTimeInterval(timeout)
        func elapsed() -> JSON { .int(Int(Date().timeIntervalSince(start) * 1000)) }
        if let label = a["label"]?.string {
            try AX.requireTrust()
            while true {
                // The frontmost app can change while waiting (a launch), so
                // look it up on every poll. Closed menus still list their
                // items, so the menu bar doesn't count (File > Save is not
                // the save sheet's button).
                if let target = try? Apps.target(a["app"]?.string),
                   LabelMatch.exists(label, in: AX.tree(AX.app(target.processIdentifier), depth: 40).root.pruned(roles: ["AXMenuBar"])) {
                    return .success(["waited_ms": elapsed()])
                }
                if Date() >= deadline { throw GuiError("timed out after \(Int(timeout)) s waiting for '\(label)'") }
                usleep(250_000)
            }
        }
        if a["idle"]?.bool == true {
            var idle = IdleDetector()
            while true {
                if idle.add(try Capture.thumbnail()) { return .success(["waited_ms": elapsed()]) }
                if Date() >= deadline { throw GuiError("timed out after \(Int(timeout)) s waiting for the screen to settle") }
                usleep(200_000)
            }
        }
        throw GuiError("wait needs label or idle")
    }
}
