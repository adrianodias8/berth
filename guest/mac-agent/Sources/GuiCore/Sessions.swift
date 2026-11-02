// session.* on macOS. A macOS VM has one screen, so a session is bookkeeping:
// who started it, when, and which apps it opened. Isolation between agents
// comes from the host giving each its own VM clone. Kept in memory; the host
// holds the durable record (~/.berth/gui/sessions.json).
import Foundation

public final class SessionBook {
    public struct Session: Equatable {
        public var id: String
        public var agent: String?
        public var since: Date
        public var apps: [String]
    }

    private var sessions: [Session] = []
    private let lock = NSLock()
    private let now: () -> Date

    public init(now: @escaping () -> Date = Date.init) { self.now = now }

    public func new(agent: String?) -> String {
        lock.lock(); defer { lock.unlock() }
        var id: String
        repeat {
            id = "s-" + String(format: "%08x", UInt32.random(in: 0...UInt32.max))
        } while sessions.contains { $0.id == id }
        sessions.append(Session(id: id, agent: agent, since: now(), apps: []))
        return id
    }

    /// Ending a session that isn't there is fine: the host may retry.
    public func end(_ id: String?) {
        guard let id else { return }
        lock.lock(); defer { lock.unlock() }
        sessions.removeAll { $0.id == id }
    }

    /// Records an app opened in a session (once per app).
    public func noteApp(_ app: String, session id: String?) {
        guard let id else { return }
        lock.lock(); defer { lock.unlock() }
        guard let i = sessions.firstIndex(where: { $0.id == id }) else { return }
        if !sessions[i].apps.contains(app) { sessions[i].apps.append(app) }
    }

    public func list() -> [Session] {
        lock.lock(); defer { lock.unlock() }
        return sessions
    }

    public func listJSON() -> JSON {
        let iso = ISO8601DateFormatter()
        return .object(["sessions": .array(list().map { s in
            .object([
                "session": .string(s.id),
                "agent": .from(s.agent),
                "since": .string(iso.string(from: s.since)),
                "apps": .array(s.apps.map { .string($0) }),
            ])
        })])
    }
}
