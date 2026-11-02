import AppKit
import ImageIO
import SwiftUI

// Agents: a watch-only peek at what coding agents do in GUI sessions
// (`berth gui`, docs/gui-protocol.md). Everything is read from files the CLI
// writes: ~/.berth/gui/sessions.json (active sessions), ~/.berth/gui/queue.json
// (macOS sessions waiting for one of Apple's two VM slots) and the activity
// log ~/.berth/logs/gui/<YYYY-MM-DD>.jsonl, one line when a command starts and
// one when it ends. Nothing here talks to a guest. "Take over" shows a macOS
// VM's screen through `berth mac start <vm> --display`; a Linux session opens
// a Display window (Display.swift). Polled every second while on screen: the
// log only grows, so each poll reads just the new bytes.

// MARK: - Model

/// Any JSON value: command args and results are free-form per verb.
enum JSONValue: Decodable, Hashable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }

    var string: String? { if case let .string(s) = self { s } else { nil } }
    var number: Double? { if case let .number(n) = self { n } else { nil } }
    var bool: Bool? { if case let .bool(b) = self { b } else { nil } }
    var array: [JSONValue]? { if case let .array(a) = self { a } else { nil } }

    var any: Any {
        switch self {
        case .null: NSNull()
        case let .bool(b): b
        case let .number(n): n
        case let .string(s): s
        case let .array(a): a.map(\.any)
        case let .object(o): o.mapValues(\.any)
        }
    }

    /// Readable JSON for the inspector; a plain string stays itself.
    var pretty: String {
        if case let .string(s) = self { return s }
        guard JSONSerialization.isValidJSONObject(any),
              let data = try? JSONSerialization.data(withJSONObject: any, options: [.prettyPrinted, .sortedKeys]) else {
            return Describe.short(self)
        }
        return String(decoding: data, as: UTF8.self)
    }
}

/// Times in the log and sessions file: RFC 3339 strings (with or without
/// fractions), or Unix seconds.
enum GuiTime {
    static func parse(_ value: JSONValue?) -> Date? {
        switch value {
        case let .number(n)?: return Date(timeIntervalSince1970: n)
        case let .string(s)?:
            let f = ISO8601DateFormatter()
            if let d = f.date(from: s) { return d }
            f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
            return f.date(from: s)
        default: return nil
        }
    }
}

/// Who ran a command (protocol: Activity log, `who`).
struct GuiWho: Decodable, Hashable {
    var kind = "terminal"
    var chat: String?
    var title: String?
    var pid: Int?
    var cwd: String?

    init(kind: String = "terminal", chat: String? = nil, title: String? = nil, pid: Int? = nil, cwd: String? = nil) {
        self.kind = kind; self.chat = chat; self.title = title; self.pid = pid; self.cwd = cwd
    }

    enum CodingKeys: String, CodingKey { case kind, chat, title, pid, cwd }

    init(from decoder: Decoder) throws {
        // A bare string is taken as the kind.
        if let kind = try? decoder.singleValueContainer().decode(String.self) {
            self.kind = kind
            return
        }
        let c = try decoder.container(keyedBy: CodingKeys.self)
        kind = (try? c.decodeIfPresent(String.self, forKey: .kind)) ?? "terminal"
        chat = try? c.decodeIfPresent(String.self, forKey: .chat)
        title = try? c.decodeIfPresent(String.self, forKey: .title)
        pid = try? c.decodeIfPresent(Int.self, forKey: .pid)
        cwd = try? c.decodeIfPresent(String.self, forKey: .cwd)
    }

    var kindName: String {
        switch kind {
        case "claude-code": "Claude Code"
        case "cursor": "Cursor"
        case "codex": "Codex"
        case "mcp": "An MCP client"
        case "terminal": "You, in a terminal"
        case "berth-app": "Berth"
        default: kind
        }
    }

    /// The chat's name: its title if the agent's transcript gave one, else
    /// the agent and the folder it works in.
    var chatName: String {
        if let title, !title.isEmpty { return title }
        if let cwd, !cwd.isEmpty { return "\(kindName) in \((cwd as NSString).lastPathComponent)" }
        return kindName
    }

    /// Commands of one chat share this: the chat id, else the process.
    var chatKey: String { chat.map { "\(kind):\($0)" } ?? "\(kind):pid \(pid.map(String.init) ?? "?"):\(cwd ?? "")" }

    var icon: String {
        switch kind {
        case "claude-code", "codex", "cursor", "mcp": "sparkles"
        default: "terminal"
        }
    }
}

/// The element a pointer action landed on.
struct GuiHit: Decodable, Hashable {
    var role: String?
    var label: String?
    var frame: [Double]?
}

/// One line of the activity log.
struct GuiEvent: Decodable {
    var id: String
    var phase: String
    var time: Date?
    var target: String
    var session: String?
    var verb: String
    var args: [String: JSONValue]
    var ok: Bool?
    var error: String?
    var durationMs: Int?
    var screenshot: String?
    var hit: GuiHit?
    var who: GuiWho?
    var result: JSONValue?

    enum CodingKeys: String, CodingKey {
        case id, phase, time, target, session, verb, args, ok, error, screenshot, hit, who, result
        case durationMs = "duration_ms"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        phase = (try? c.decodeIfPresent(String.self, forKey: .phase)) ?? "end"
        time = GuiTime.parse(try? c.decodeIfPresent(JSONValue.self, forKey: .time))
        target = (try? c.decodeIfPresent(String.self, forKey: .target)) ?? ""
        session = try? c.decodeIfPresent(String.self, forKey: .session)
        verb = (try? c.decodeIfPresent(String.self, forKey: .verb)) ?? "?"
        args = (try? c.decodeIfPresent([String: JSONValue].self, forKey: .args)) ?? [:]
        ok = try? c.decodeIfPresent(Bool.self, forKey: .ok)
        error = try? c.decodeIfPresent(String.self, forKey: .error)
        durationMs = (try? c.decodeIfPresent(Double.self, forKey: .durationMs)).map { Int($0) }
        screenshot = try? c.decodeIfPresent(String.self, forKey: .screenshot)
        hit = try? c.decodeIfPresent(GuiHit.self, forKey: .hit)
        who = try? c.decodeIfPresent(GuiWho.self, forKey: .who)
        result = try? c.decodeIfPresent(JSONValue.self, forKey: .result)
    }
}

/// A command: its start and end lines merged.
struct GuiCommand: Identifiable, Hashable {
    let id: String
    var target: String
    var session: String?
    var verb: String
    var args: [String: JSONValue]
    var who: GuiWho
    var started: Date?
    var ended: Date?
    var ok: Bool?
    var error: String?
    var durationMs: Int?
    var screenshot: String?
    var hit: GuiHit?
    var result: JSONValue?
    /// The app it concerns: named in its args, else the one last opened or
    /// focused in its session (ActivityLog.assignApps).
    var app: String?

    init(_ e: GuiEvent) {
        id = e.id; target = e.target; session = e.session; verb = e.verb; args = e.args
        who = e.who ?? GuiWho()
        started = e.phase == "start" ? e.time : nil
        app = nil
        apply(e)
    }

    mutating func apply(_ e: GuiEvent) {
        if e.phase == "start" {
            started = e.time ?? started
        } else {
            ended = e.time ?? ended ?? started
            ok = e.ok ?? (e.error == nil)
            error = e.error ?? error
            durationMs = e.durationMs ?? durationMs
        }
        if !e.args.isEmpty { args = e.args }
        if let s = e.session { session = s }
        if !e.target.isEmpty { target = e.target }
        if let w = e.who { who = w }
        screenshot = e.screenshot ?? screenshot
        hit = e.hit ?? hit
        result = e.result ?? result
    }

    var time: Date? { ended ?? started }
    var isLinux: Bool { target.hasPrefix("linux:") }
    var failed: Bool { ok == false }

    /// Started and not ended. A start with no end for ten minutes is a CLI
    /// that died, not a command still running.
    func isRunning(now: Date = Date()) -> Bool {
        guard ended == nil, ok == nil else { return false }
        guard let started else { return true }
        return now.timeIntervalSince(started) < 600
    }

    var summary: String { Describe.command(verb, args, hit: hit, linux: isLinux) }
}

/// One entry of ~/.berth/gui/sessions.json.
struct GuiSession: Decodable, Identifiable, Hashable {
    var session: String
    var target: String
    var agent: GuiWho
    var clone: Bool
    var started: Date?
    var lastUsed: Date?
    var note: String?
    var id: String { session }
    var isLinux: Bool { target.hasPrefix("linux:") }
    /// "test" from "mac:test", "dev" from "linux:dev".
    var targetName: String { target.split(separator: ":", maxSplits: 1).last.map(String.init) ?? target }

    enum CodingKeys: String, CodingKey {
        case session, target, agent, clone, started, note
        case lastUsed = "last_used"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        session = try c.decode(String.self, forKey: .session)
        target = (try? c.decodeIfPresent(String.self, forKey: .target)) ?? ""
        agent = (try? c.decodeIfPresent(GuiWho.self, forKey: .agent)) ?? GuiWho()
        clone = (try? c.decodeIfPresent(Bool.self, forKey: .clone)) ?? false
        started = GuiTime.parse(try? c.decodeIfPresent(JSONValue.self, forKey: .started))
        lastUsed = GuiTime.parse(try? c.decodeIfPresent(JSONValue.self, forKey: .lastUsed))
        note = try? c.decodeIfPresent(String.self, forKey: .note)
    }
}

// MARK: - Plain language

/// What a command did, the way a person would say it: "Opened MyApp",
/// "Clicked “Save”", "Pressed ⌘A". Pure functions, so they're testable.
enum Describe {
    /// `running` gives the present tense for a command under way.
    static func command(_ verb: String, _ args: [String: JSONValue], hit: GuiHit?, linux: Bool, running: Bool = false) -> String {
        func say(_ past: String, _ now: String) -> String { running ? now : past }
        let app = args["app"]?.string.map(appName)
        switch verb {
        case "hello":
            return say("Connected to the VM", "Connecting to the VM…")
        case "session.new":
            return say("Started a session", "Starting a session…")
        case "session.end":
            return say("Ended the session", "Ending the session…")
        case "session.list":
            return say("Listed the sessions", "Listing the sessions…")
        case "push":
            let name = args["name"]?.string.map { ($0 as NSString).lastPathComponent } ?? "files"
            return say("Copied \(name) into the VM", "Copying \(name) into the VM…")
        case "open":
            let name = app ?? "an app"
            return say("Opened \(name)", "Opening \(name)…")
        case "screenshot":
            if let window = args["window"]?.string.map(appName) {
                return say("Took a screenshot of \(window)", "Taking a screenshot of \(window)…")
            }
            return say("Took a screenshot", "Taking a screenshot…")
        case "click":
            let count = Int(args["count"]?.number ?? 1)
            let right = args["button"]?.string == "right"
            let (past, now): (String, String) = switch (right, count) {
            case (true, _): ("Right-clicked", "Right-clicking")
            case (false, 2): ("Double-clicked", "Double-clicking")
            case (false, 3...): ("Triple-clicked", "Triple-clicking")
            default: ("Clicked", "Clicking")
            }
            let what = clickTarget(args, hit: hit)
            return say("\(past) \(what)", "\(now) \(what)…")
        case "type":
            let text = quoted(args["text"]?.string ?? "")
            return say("Typed \(text)", "Typing \(text)…")
        case "key":
            let combo = keyCombo(args["combo"]?.string ?? "", linux: linux)
            return say("Pressed \(combo)", "Pressing \(combo)…")
        case "scroll":
            let direction = scrollDirection(dx: args["dx"]?.number ?? 0, dy: args["dy"]?.number ?? 0)
            let place = hit?.label.flatMap { $0.isEmpty ? nil : " in \(quoted($0))" } ?? ""
            return say("Scrolled \(direction)\(place)", "Scrolling \(direction)\(place)…")
        case "drag":
            let from = point(args["from"]), to = point(args["to"])
            let what = hit?.label.flatMap { $0.isEmpty ? nil : "\(quoted($0)) " } ?? ""
            return say("Dragged \(what)from \(from) to \(to)", "Dragging \(what)from \(from) to \(to)…")
        case "tree":
            if let app { return say("Read \(app)'s accessibility tree", "Reading \(app)'s accessibility tree…") }
            return say("Read the screen's accessibility tree", "Reading the screen's accessibility tree…")
        case "wait":
            if let label = args["label"]?.string {
                return say("Waited for \(quoted(label))", "Waiting for \(quoted(label))…")
            }
            return say("Waited for the screen to settle", "Waiting for the screen to settle…")
        case "windows":
            return say("Listed the open windows", "Listing the open windows…")
        case "focus":
            let name = app ?? "an app"
            return say("Brought \(name) to the front", "Bringing \(name) to the front…")
        case "quit":
            let name = app ?? "an app"
            return say("Quit \(name)", "Quitting \(name)…")
        case "note":
            return "Note: " + (args["text"]?.string ?? args["note"]?.string ?? "")
        default:
            return say(verb, "\(verb)…")
        }
    }

    /// “Save” by label; else the element hit; else the coordinates.
    static func clickTarget(_ args: [String: JSONValue], hit: GuiHit?) -> String {
        if let label = args["label"]?.string, !label.isEmpty { return quoted(label) }
        if let label = hit?.label, !label.isEmpty { return quoted(label) }
        if let x = args["x"]?.number, let y = args["y"]?.number { return "at \(num(x)), \(num(y))" }
        return "on the screen"
    }

    /// "MyApp" from "/Applications/MyApp.app", "MyApp.app" or "org.x.MyApp.desktop".
    static func appName(_ path: String) -> String {
        var name = (path as NSString).lastPathComponent
        for suffix in [".app", ".desktop"] where name.hasSuffix(suffix) && name.count > suffix.count {
            name.removeLast(suffix.count)
        }
        return name.isEmpty ? path : name
    }

    /// Curly quotes around at most `limit` characters, newlines as ↩.
    static func quoted(_ text: String, limit: Int = 40) -> String {
        var shown = text.replacingOccurrences(of: "\r\n", with: "↩").replacingOccurrences(of: "\n", with: "↩")
            .replacingOccurrences(of: "\t", with: "⇥")
        if shown.count > limit { shown = String(shown.prefix(limit)) + "…" }
        return "“\(shown)”"
    }

    /// "cmd+shift+a" → "⇧⌘A" on a Mac (modifiers in Apple's order ⌃⌥⇧⌘),
    /// "Super+Shift+A" on Linux, where the protocol's `cmd` is the super key.
    static func keyCombo(_ combo: String, linux: Bool) -> String {
        let parts = combo.split(separator: "+").map { $0.trimmingCharacters(in: .whitespaces).lowercased() }.filter { !$0.isEmpty }
        guard !parts.isEmpty else { return "a key" }
        // Mac order, and the Linux name, of each modifier.
        let modifiers: [(names: [String], mac: String, linux: String)] = [
            (["ctrl", "control"], "⌃", "Ctrl"),
            (["alt", "option", "opt"], "⌥", "Alt"),
            (["shift"], "⇧", "Shift"),
            (["cmd", "command", "super", "meta", "win"], "⌘", "Super"),
        ]
        var held: [String] = []
        var keys: [String] = []
        for part in parts {
            if modifiers.contains(where: { $0.names.contains(part) }) { held.append(part) } else { keys.append(part) }
        }
        let mods = modifiers.filter { m in held.contains { m.names.contains($0) } }.map { linux ? $0.linux : $0.mac }
        let key = keys.map { keyName($0, linux: linux) }.joined(separator: linux ? "+" : "")
        if linux { return (mods + (key.isEmpty ? [] : [key])).joined(separator: "+") }
        return mods.joined() + key
    }

    static func keyName(_ key: String, linux: Bool) -> String {
        let names: [String: (mac: String, linux: String)] = [
            "return": ("↩", "Enter"), "enter": ("↩", "Enter"), "tab": ("⇥", "Tab"),
            "esc": ("⎋", "Esc"), "escape": ("⎋", "Esc"), "space": ("Space", "Space"),
            "delete": ("⌫", "Backspace"), "backspace": ("⌫", "Backspace"),
            "forwarddelete": ("⌦", "Delete"), "del": ("⌦", "Delete"),
            "up": ("↑", "Up"), "down": ("↓", "Down"), "left": ("←", "Left"), "right": ("→", "Right"),
            "home": ("↖", "Home"), "end": ("↘", "End"), "pageup": ("⇞", "Page Up"), "pagedown": ("⇟", "Page Down"),
            "fn": ("fn", "Fn"),
        ]
        if let n = names[key] { return linux ? n.linux : n.mac }
        return key.count == 1 ? key.uppercased() : key.prefix(1).uppercased() + key.dropFirst()
    }

    /// Screen coordinates grow downwards: a positive dy scrolls down.
    static func scrollDirection(dx: Double, dy: Double) -> String {
        if dy == 0 && dx == 0 { return "" }
        if abs(dy) >= abs(dx) { return dy > 0 ? "down" : "up" }
        return dx > 0 ? "right" : "left"
    }

    static func point(_ value: JSONValue?) -> String {
        guard let a = value?.array, a.count >= 2, let x = a[0].number, let y = a[1].number else { return "?" }
        return "\(num(x)), \(num(y))"
    }

    /// 120 rather than 120.0.
    static func num(_ d: Double) -> String {
        d == d.rounded() && abs(d) < 1e15 ? String(Int(d)) : String(format: "%.1f", d)
    }

    static func short(_ value: JSONValue) -> String {
        switch value {
        case .null: "null"
        case let .bool(b): b ? "true" : "false"
        case let .number(n): num(n)
        case let .string(s): s
        case let .array(a): a.map(short).joined(separator: ",")
        case let .object(o): "{" + o.keys.sorted().map { "\($0): \(short(o[$0]!))" }.joined(separator: ", ") + "}"
        }
    }

    /// The call as a `berth gui` command line, for the inspector.
    static func commandLine(_ c: GuiCommand) -> String {
        var words = ["berth", "gui"] + c.verb.split(separator: ".").map(String.init)
        var rest = c.args
        func take(_ key: String) -> JSONValue? { rest.removeValue(forKey: key) }
        switch c.verb {
        case "click":
            if let label = take("label")?.string { words.append(label) }
            else if let x = rest["x"]?.number, let y = rest["y"]?.number {
                rest["x"] = nil; rest["y"] = nil
                words.append("\(num(x)),\(num(y))")
            }
        case "type": if let t = take("text") { words.append(short(t)) }
        case "key": if let k = take("combo") { words.append(short(k)) }
        case "open":
            if let a = take("app") { words.append(short(a)) }
            if let extra = take("args")?.array, !extra.isEmpty { words += ["--"] + extra.map(short) }
        case "push": if let n = take("name") { words.append(short(n)) }
        case "focus", "quit": if let a = take("app") { words.append(short(a)) }
        case "wait": if let l = take("label") { words.append(short(l)) }
        case "note": if let t = take("text") ?? take("note") { words.append(short(t)) }
        case "session.new": rest["agent"] = nil
        default: break
        }
        for key in rest.keys.sorted() {
            let flag = "--" + key.replacingOccurrences(of: "_", with: "-")
            switch rest[key]! {
            case .null, .bool(false): continue
            case .bool(true): words.append(flag)
            case let v: words += [flag, short(v)]
            }
        }
        if !c.target.isEmpty { words += ["--target", c.target] }
        if let s = c.session { words += ["--session", s] }
        return words.enumerated().map { i, w in i < 2 ? w : shellQuote(w) }.joined(separator: " ")
    }

    static func shellQuote(_ word: String) -> String {
        let safe = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_./:,=@+%")
        if !word.isEmpty && word.unicodeScalars.allSatisfy(safe.contains) { return word }
        return "'" + word.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// "4 s ago", "3 min ago", or the time of day.
    static func ago(_ date: Date?, now: Date = Date()) -> String {
        guard let date else { return "" }
        let s = Int(now.timeIntervalSince(date))
        if s < 5 { return "just now" }
        if s < 60 { return "\(s) s ago" }
        if s < 3600 { return "\(s / 60) min ago" }
        return date.formatted(date: s < 86_400 ? .omitted : .abbreviated, time: .shortened)
    }

    static func duration(_ ms: Int?) -> String {
        guard let ms else { return "" }
        return ms < 1000 ? "\(ms) ms" : String(format: "%.1f s", Double(ms) / 1000)
    }
}

// MARK: - Reading the log

enum ActivityLog {
    /// Merge log lines into commands keyed by id, keeping first-seen order.
    static func merge(_ events: [GuiEvent], into commands: inout [String: GuiCommand], order: inout [String]) {
        for e in events {
            if commands[e.id] != nil {
                commands[e.id]!.apply(e)
            } else {
                commands[e.id] = GuiCommand(e)
                order.append(e.id)
            }
        }
    }

    /// The app each command concerns: named in its args, else the one last
    /// opened, focused or pushed in the same session (or target).
    static func assignApps(_ commands: [GuiCommand]) -> [GuiCommand] {
        var current: [String: String] = [:]
        return commands.map { c in
            var c = c
            let key = c.session ?? c.target
            let named = (c.args["app"]?.string ?? (c.verb == "screenshot" ? c.args["window"]?.string : nil)
                ?? (c.verb == "push" ? c.args["name"]?.string : nil)).map(Describe.appName)
            if let named, ["open", "focus", "push"].contains(c.verb) { current[key] = named }
            c.app = named ?? current[key]
            if c.verb == "quit", current[key] == named { current[key] = nil }
            return c
        }
    }

    /// Lines of a JSONL chunk that decode; others (a line being written,
    /// one from a newer berth) are skipped.
    static func decode(_ lines: [Data]) -> [GuiEvent] {
        let decoder = JSONDecoder()
        return lines.compactMap { try? decoder.decode(GuiEvent.self, from: $0) }
    }
}

/// Reads what was appended to files since the last call, whole lines only
/// (a line still being written waits for the next call). One instance is
/// used from one task at a time.
final class LogTail: @unchecked Sendable {
    private var offsets: [String: UInt64] = [:]
    private var partial: [String: Data] = [:]

    func read(_ url: URL) -> [Data] {
        let path = url.path
        guard let handle = try? FileHandle(forReadingFrom: url) else { return [] }
        defer { try? handle.close() }
        let size = (try? handle.seekToEnd()) ?? 0
        var offset = offsets[path] ?? 0
        if size < offset {
            // Truncated or replaced: start over.
            offset = 0
            partial[path] = nil
        }
        guard size > offset else { return [] }
        try? handle.seek(toOffset: offset)
        let data = (partial[path] ?? Data()) + ((try? handle.readToEnd()) ?? Data())
        offsets[path] = size
        var lines = data.split(separator: 0x0a, omittingEmptySubsequences: true).map { Data($0) }
        if data.last != 0x0a, let last = lines.popLast() { partial[path] = last } else { partial[path] = nil }
        return lines
    }
}

/// Where the files are; a different root for tests and previews.
struct GuiFiles {
    let root: URL
    var sessions: URL { root.appendingPathComponent("gui/sessions.json") }
    var queue: URL { root.appendingPathComponent("gui/queue.json") }
    var logs: URL { root.appendingPathComponent("logs/gui") }

    func readSessions() -> [GuiSession] {
        guard let data = try? Data(contentsOf: sessions) else { return [] }
        return ((try? JSONDecoder().decode([GuiSession].self, from: data)) ?? [])
            .sorted { ($0.started ?? .distantPast) < ($1.started ?? .distantPast) }
    }

    /// How many sessions wait for a macOS VM slot.
    func queued() -> Int {
        guard let data = try? Data(contentsOf: queue) else { return 0 }
        return ((try? JSONSerialization.jsonObject(with: data)) as? [Any])?.count ?? 0
    }

    /// The newest `days` log files, oldest first.
    func logFiles(days: Int = 7) -> [URL] {
        let names = ((try? FileManager.default.contentsOfDirectory(atPath: logs.path)) ?? [])
            .filter { $0.hasSuffix(".jsonl") }.sorted()
        return names.suffix(days).map { logs.appendingPathComponent($0) }
    }
}

/// Sessions and commands, refreshed while the Agents screen is shown.
@MainActor
final class AgentActivity: ObservableObject {
    @Published private(set) var sessions: [GuiSession] = []
    /// Oldest first, apps assigned; the app's own Display refreshes left out.
    @Published private(set) var commands: [GuiCommand] = []
    @Published private(set) var queued = 0

    let files: GuiFiles
    private let tail = LogTail()
    private var byID: [String: GuiCommand] = [:]
    private var order: [String] = []
    /// Enough for days of work without the timeline getting slow.
    private let limit = 5000

    init(root: URL = BerthPaths.root) { files = GuiFiles(root: root) }

    func refresh() async {
        let files = files, tail = tail
        let (sessions, queued, events) = await offPool { () -> ([GuiSession], Int, [GuiEvent]) in
            let lines = files.logFiles().flatMap { tail.read($0) }
            return (files.readSessions(), files.queued(), ActivityLog.decode(lines))
        }
        if sessions != self.sessions { self.sessions = sessions }
        if queued != self.queued { self.queued = queued }
        guard !events.isEmpty else { return }
        ActivityLog.merge(events.filter { $0.who?.kind != "berth-app" }, into: &byID, order: &order)
        if order.count > limit {
            for id in order.prefix(order.count - limit) { byID[id] = nil }
            order.removeFirst(order.count - limit)
        }
        let merged = ActivityLog.assignApps(order.compactMap { byID[$0] })
        if merged != commands { commands = merged }
    }

    func commands(in session: String) -> [GuiCommand] { commands.filter { $0.session == session } }

    /// The newest command of a session, its newest screenshot and note.
    func latest(_ session: String) -> (command: GuiCommand?, screenshot: String?, note: String?) {
        let mine = commands(in: session)
        let note = mine.last(where: { $0.verb == "note" }).flatMap { $0.args["text"]?.string ?? $0.args["note"]?.string }
        return (mine.last(where: { $0.verb != "note" }) ?? mine.last, mine.last(where: { $0.screenshot != nil })?.screenshot, note)
    }

    /// Apps seen in the log, for the filter.
    var apps: [String] { Array(Set(commands.compactMap(\.app))).sorted { $0.localizedCaseInsensitiveCompare($1) == .orderedAscending } }
}

// MARK: - Screenshots

/// Screenshots shrunk to the size they're shown at, decoded off the main
/// thread and cached (a timeline has hundreds).
enum Shots {
    private static let cache = NSCache<NSString, NSImage>()

    static func load(_ path: String, maxPixel: Int) async -> NSImage? {
        let key = "\(path)#\(maxPixel)" as NSString
        if let hit = cache.object(forKey: key) { return hit }
        let image = await offPool { () -> NSImage? in
            guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil) else { return nil }
            let options: [CFString: Any] = [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceThumbnailMaxPixelSize: maxPixel,
            ]
            guard let cg = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { return nil }
            return NSImage(cgImage: cg, size: NSSize(width: cg.width, height: cg.height))
        }
        if let image { cache.setObject(image, forKey: key) }
        return image
    }
}

struct ShotImage: View {
    let path: String?
    var maxPixel = 480
    @State private var image: NSImage?

    var body: some View {
        ZStack {
            Rectangle().fill(.quaternary.opacity(0.6))
            if let image {
                Image(nsImage: image).resizable().aspectRatio(contentMode: .fit)
            } else {
                Image(systemName: path == nil ? "rectangle.dashed" : "photo").foregroundStyle(.tertiary)
            }
        }
        .task(id: path) {
            image = nil
            if let path { image = await Shots.load(path, maxPixel: maxPixel) }
        }
        .accessibilityLabel(path == nil ? "No screenshot" : "Screenshot")
    }
}

// MARK: - Views

struct AgentsView: View {
    enum Mode: String, CaseIterable { case live = "Live", timeline = "Timeline" }

    @EnvironmentObject var engine: Engine
    @StateObject private var activity: AgentActivity
    // `-agentsMode Timeline` starts there (scripted screenshots).
    @State private var mode = Mode(rawValue: UserDefaults.standard.string(forKey: "agentsMode") ?? "") ?? .live
    @State private var chat: String?
    // `-openCommand <id>` shows one in the inspector.
    @State private var selected: GuiCommand.ID? = UserDefaults.standard.string(forKey: "openCommand")
    /// "" is every app.
    @State private var app = ""
    @Environment(\.openWindow) private var openWindow

    init(root: URL = BerthPaths.root) { _activity = StateObject(wrappedValue: AgentActivity(root: root)) }

    var body: some View {
        Group {
            switch mode {
            case .live: live
            case .timeline: timeline
            }
        }
        .navigationTitle("Agents")
        .toolbar {
            ToolbarItem(placement: .principal) {
                Picker("View", selection: $mode) {
                    ForEach(Mode.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                }
                .pickerStyle(.segmented)
                .fixedSize()
            }
            if mode == .timeline {
                ToolbarItem {
                    Picker("App", selection: $app) {
                        Text("All Apps").tag("")
                        ForEach(activity.apps, id: \.self) { Text($0).tag($0) }
                    }
                    .help("Only commands about this app")
                }
            }
        }
        .task {
            while !Task.isCancelled {
                await activity.refresh()
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    // MARK: Live

    private var live: some View {
        Group {
            if activity.sessions.isEmpty {
                ContentUnavailableView {
                    Label("No Agents at Work", systemImage: "eye")
                } description: {
                    Text("When an agent starts a session (`berth gui session new`), its screen and what it's doing show here. Watch-only: nothing here clicks in its apps.")
                } actions: {
                    if !activity.commands.isEmpty { Button("Show Timeline") { mode = .timeline } }
                }
            } else {
                ScrollView {
                    if activity.queued > 0 {
                        Label("\(activity.queued) waiting for a macOS VM slot (Apple allows \(MacVMs.runningLimit) running at once)",
                              systemImage: "hourglass")
                            .font(.callout).foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding([.horizontal, .top], 16)
                    }
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 280, maximum: 420), spacing: 16, alignment: .top)], spacing: 16) {
                        ForEach(activity.sessions) { s in
                            let latest = activity.latest(s.session)
                            SessionCard(session: s, latest: latest.command, screenshot: latest.screenshot, loggedNote: latest.note,
                                        showTimeline: {
                                            chat = latest.command?.who.chatKey ?? s.agent.chatKey
                                            selected = latest.command?.id
                                            mode = .timeline
                                        },
                                        takeOver: { takeOver(s) })
                        }
                    }
                    .padding(16)
                }
            }
        }
    }

    /// macOS: the VM's own window. Linux: a Display window (screenshots
    /// until the GPU display lands).
    private func takeOver(_ s: GuiSession) {
        if s.isLinux {
            openWindow(id: "display", value: DisplayTarget(target: s.target, session: s.session))
        } else {
            Task {
                if let error = await MacVMs.run(["start", s.targetName, "--display"]) {
                    engine.lastError = "Take over \(s.targetName): \(error)"
                }
            }
        }
    }

    // MARK: Timeline

    private struct Chat: Identifiable {
        let id: String
        let who: GuiWho
        let last: Date?
        let count: Int
        let failed: Int
    }

    private var filtered: [GuiCommand] {
        app.isEmpty ? activity.commands : activity.commands.filter { $0.app == app }
    }

    private var chats: [Chat] {
        Dictionary(grouping: filtered, by: \.who.chatKey).map { key, cs in
            Chat(id: key, who: cs.last!.who, last: cs.last!.time, count: cs.count, failed: cs.filter(\.failed).count)
        }
        .sorted { ($0.last ?? .distantPast) > ($1.last ?? .distantPast) }
    }

    private var timeline: some View {
        let chats = chats
        let current = chat.flatMap { key in chats.first { $0.id == key } } ?? chats.first
        let rows = current.map { c in filtered.filter { $0.who.chatKey == c.id } } ?? []
        return HStack(spacing: 0) {
            List(selection: Binding(get: { current?.id }, set: { chat = $0; selected = nil })) {
                ForEach(chats) { c in
                    VStack(alignment: .leading, spacing: 2) {
                        Text(c.who.chatName).fontWeight(.medium).lineLimit(2)
                        Text("\(c.who.kindName) · \(c.count) command\(c.count == 1 ? "" : "s")\(c.failed > 0 ? " · \(c.failed) failed" : "") · \(Describe.ago(c.last))")
                            .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    .padding(.vertical, 2)
                    .tag(c.id)
                }
            }
            .overlay {
                if chats.isEmpty {
                    if app.isEmpty {
                        ContentUnavailableView("No Activity Yet", systemImage: "list.bullet.rectangle",
                                               description: Text("Every `berth gui` command is listed here with who ran it."))
                    } else {
                        ContentUnavailableView.search(text: app)
                    }
                }
            }
            .frame(width: 240)
            Divider()
            ScrollViewReader { proxy in
                List(selection: $selected) {
                    ForEach(rows) { CommandRow(command: $0).tag($0.id) }
                }
                .onAppear { if let last = rows.last { proxy.scrollTo(last.id, anchor: .bottom) } }
                .onChange(of: rows.last?.id) { _, id in if let id, selected == nil { proxy.scrollTo(id, anchor: .bottom) } }
            }
            .frame(minWidth: 280, maxWidth: .infinity)
            if let id = selected, let c = activity.commands.first(where: { $0.id == id }) {
                Divider()
                CommandInspector(command: c).frame(width: 340)
            }
        }
    }
}

private struct SessionCard: View {
    let session: GuiSession
    let latest: GuiCommand?
    let screenshot: String?
    /// The newest `berth gui note`, when sessions.json has none.
    let loggedNote: String?
    let showTimeline: () -> Void
    let takeOver: () -> Void

    private var who: GuiWho { latest?.who ?? session.agent }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ShotImage(path: screenshot, maxPixel: 840)
                .aspectRatio(16 / 10, contentMode: .fit)
                .clipShape(RoundedRectangle(cornerRadius: 6))
                .overlay(alignment: .topLeading) {
                    if latest?.isRunning() == true {
                        Text("LIVE").font(.caption2.weight(.bold)).padding(.horizontal, 5).padding(.vertical, 2)
                            .background(.red, in: Capsule()).foregroundStyle(.white).padding(6)
                    }
                }
                .onTapGesture(count: 2, perform: showTimeline)
            HStack(spacing: 6) {
                Image(systemName: who.icon).foregroundStyle(.secondary)
                Text(who.kindName).font(.caption).foregroundStyle(.secondary)
                Spacer()
                Text("\(session.target)\(session.clone ? " (clone)" : "")").font(.caption.monospaced()).foregroundStyle(.secondary)
            }
            Text(who.chatName).font(.headline).lineLimit(2)
            if let note = session.note ?? loggedNote, !note.isEmpty {
                Text(note).font(.callout).italic().foregroundStyle(.secondary).lineLimit(3)
            }
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if let c = latest {
                    let running = c.isRunning()
                    Image(systemName: running ? "circle.dotted" : c.failed ? "xmark.circle.fill" : "checkmark.circle")
                        .foregroundStyle(running ? Color.accentColor : c.failed ? .red : .secondary)
                    Text(Describe.command(c.verb, c.args, hit: c.hit, linux: c.isLinux, running: running)).lineLimit(2)
                    Spacer()
                    // Re-rendered on its own: nothing else changes while an agent thinks.
                    TimelineView(.periodic(from: .now, by: 5)) { context in
                        Text(Describe.ago(c.time, now: context.date)).font(.caption).foregroundStyle(.secondary)
                    }
                } else {
                    Text("No commands yet").foregroundStyle(.secondary)
                }
            }
            .font(.callout)
            HStack {
                Button("Timeline", action: showTimeline)
                Spacer()
                Button(session.isLinux ? "Watch" : "Take Over", action: takeOver)
                    .help(session.isLinux
                        ? "Linux sessions show as refreshed screenshots until the GPU display lands; watch-only"
                        : "Shows the VM's screen in a window you can use. The agent keeps running.")
            }
            .controlSize(.small)
        }
        .padding(10)
        .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 10))
        .contextMenu {
            Button("Copy Session ID") { copy(session.session) }
            if let cwd = who.cwd { Button("Copy Agent's Folder") { copy(cwd) } }
        }
    }
}

private struct CommandRow: View {
    let command: GuiCommand

    var body: some View {
        HStack(spacing: 10) {
            ShotImage(path: command.screenshot, maxPixel: 160)
                .frame(width: 64, height: 40)
                .clipShape(RoundedRectangle(cornerRadius: 4))
            VStack(alignment: .leading, spacing: 2) {
                let running = command.isRunning()
                Text(Describe.command(command.verb, command.args, hit: command.hit, linux: command.isLinux, running: running))
                    .lineLimit(2)
                    .foregroundStyle(command.verb == "note" ? .secondary : .primary)
                    .italic(command.verb == "note")
                HStack(spacing: 6) {
                    Text(command.time.map { $0.formatted(date: .omitted, time: .standard) } ?? "")
                    if let ms = command.durationMs { Text("· \(Describe.duration(ms))") }
                    if let app = command.app { Text("· \(app)") }
                    Text("· \(command.target)")
                }
                .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                if let error = command.error {
                    Text(error).font(.caption).foregroundStyle(.red).lineLimit(2)
                }
            }
            Spacer()
            if command.isRunning() { ProgressView().controlSize(.small) }
            else if command.failed { Image(systemName: "xmark.circle.fill").foregroundStyle(.red).accessibilityLabel("Failed") }
        }
        .padding(.vertical, 2)
    }
}

private struct CommandInspector: View {
    let command: GuiCommand

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                if let path = command.screenshot {
                    ShotImage(path: path, maxPixel: 2000)
                        .clipShape(RoundedRectangle(cornerRadius: 6))
                        .onTapGesture(count: 2) { NSWorkspace.shared.open(URL(fileURLWithPath: path)) }
                        .help("Double-click to open it in Preview")
                    HStack {
                        Button("Open") { NSWorkspace.shared.open(URL(fileURLWithPath: path)) }
                        Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) }
                    }
                    .controlSize(.small)
                }
                Text(command.summary).font(.headline).textSelection(.enabled)
                Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 4) {
                    row("When", command.time.map { $0.formatted(date: .abbreviated, time: .standard) })
                    row("Took", command.durationMs.map { Describe.duration($0) } ?? (command.isRunning() ? "running…" : nil))
                    row("Target", command.target)
                    row("Session", command.session)
                    row("App", command.app)
                    row("Agent", command.who.kindName + (command.who.pid.map { " (pid \($0))" } ?? ""))
                    row("Chat", command.who.title ?? command.who.chat)
                    row("Folder", command.who.cwd)
                }
                .font(.callout)
                block("Command", Describe.commandLine(command), copyable: true)
                if let hit = command.hit {
                    block("Element", [hit.role, hit.label.map { Describe.quoted($0, limit: 200) },
                                      hit.frame.map { f in f.map(Describe.num).joined(separator: ", ") }.map { "at [\($0)]" }]
                        .compactMap { $0 }.joined(separator: " "))
                }
                if let error = command.error {
                    block("Error", error, color: .red)
                }
                if let result = command.result, result != .null, result != .object([:]) {
                    block("Output", result.pretty)
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    @ViewBuilder
    private func row(_ label: String, _ value: String?) -> some View {
        if let value, !value.isEmpty {
            GridRow {
                Text(label).foregroundStyle(.secondary)
                Text(value).textSelection(.enabled).lineLimit(3)
            }
        }
    }

    private func block(_ title: String, _ text: String, copyable: Bool = false, color: Color = .primary) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(title).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                Spacer()
                if copyable {
                    Button { copy(text) } label: { Image(systemName: "doc.on.doc") }
                        .buttonStyle(.borderless).help("Copy").accessibilityLabel("Copy \(title)")
                }
            }
            Text(text).font(.system(.caption, design: .monospaced)).foregroundStyle(color)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(6)
                .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 5))
        }
    }
}

// MARK: - Menu bar

/// Sessions and the slot queue for the menu bar panel, read every few
/// seconds only while a GUI flag is on.
@MainActor
final class GuiMenuState: ObservableObject {
    static let shared = GuiMenuState()
    @Published private(set) var sessions: [GuiSession] = []
    @Published private(set) var queued = 0
    private var timer: Timer?

    private init() {
        timer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
            Task { @MainActor in await self?.reload() }
        }
        Task { await reload() }
    }

    private func reload() async {
        guard FeatureFlags.shared.agents else {
            if !sessions.isEmpty { sessions = [] }
            if queued != 0 { queued = 0 }
            return
        }
        let files = GuiFiles(root: BerthPaths.root)
        let (s, q) = await offPool { (files.readSessions(), files.queued()) }
        if s != sessions { sessions = s }
        if q != queued { queued = q }
    }
}

/// Nothing at all (no divider, no spacing) unless a flag is on and there's
/// a session or a queued one.
struct AgentsMenuSection: View {
    @ObservedObject private var flags = FeatureFlags.shared
    @ObservedObject private var state = GuiMenuState.shared
    @AppStorage("selectedSection") private var section: Section = .containers
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        if flags.agents && (!state.sessions.isEmpty || state.queued > 0) {
            Divider()
            VStack(alignment: .leading, spacing: 4) {
                Text(state.sessions.count == 1 ? "1 agent session" : "\(state.sessions.count) agent sessions")
                    .font(.caption).foregroundStyle(.secondary)
                ForEach(state.sessions.prefix(6)) { s in
                    Button {
                        section = .agents
                        openWindow(id: "main")
                        NSApp.activate(ignoringOtherApps: true)
                    } label: {
                        HStack {
                            Image(systemName: "eye").foregroundStyle(.secondary)
                            Text(s.agent.chatName).lineLimit(1)
                            Spacer()
                            Text(s.target).font(.caption.monospaced()).foregroundStyle(.secondary)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .frame(height: 22)
                }
                if state.queued > 0 {
                    Label("\(state.queued) waiting for a macOS VM slot", systemImage: "hourglass")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            .font(.callout)
        }
    }
}
