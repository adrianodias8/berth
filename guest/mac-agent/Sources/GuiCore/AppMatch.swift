// Finding a running app by what an agent calls it: "TextEdit", "textedit",
// "com.apple.TextEdit", "/Applications/TextEdit.app" or "TextEdit.app" all
// name the same thing. The executable fills AppInfo from NSRunningApplication.
import Foundation

public struct AppInfo: Equatable {
    public var pid: Int32
    public var name: String?
    public var bundleID: String?
    /// The bundle's path, e.g. /Applications/TextEdit.app.
    public var path: String?
    public var active: Bool

    public init(pid: Int32, name: String?, bundleID: String?, path: String?, active: Bool = false) {
        self.pid = pid
        self.name = name
        self.bundleID = bundleID
        self.path = path
        self.active = active
    }

    /// The bundle's file name without ".app".
    public var bundleName: String? {
        guard let path else { return nil }
        let last = (path as NSString).lastPathComponent
        return last.lowercased().hasSuffix(".app") ? String(last.dropLast(4)) : last
    }
}

public enum AppMatch {
    /// The running app `query` names. Several processes can share a name
    /// (helpers, two copies of one app); the active one wins, then the lowest
    /// pid (the one started first).
    public static func find(_ query: String, in apps: [AppInfo]) throws -> AppInfo {
        let q = query.trimmingCharacters(in: .whitespaces)
        guard !q.isEmpty else { throw GuiError("empty app name") }
        if let pid = Int32(q), let a = apps.first(where: { $0.pid == pid }) { return a }
        let lower = q.lowercased()
        let stripped = lower.hasSuffix(".app") ? String(lower.dropLast(4)) : lower
        let tests: [(AppInfo) -> Bool] = [
            { $0.path.map { ($0 as NSString).standardizingPath.lowercased() } == (lower as NSString).standardizingPath },
            { $0.bundleID?.lowercased() == lower },
            { $0.name?.lowercased() == stripped },
            { $0.bundleName?.lowercased() == stripped },
            // "Berth Hello" (a window title) for "BerthHello".
            { squeezed($0.name) == squeezed(stripped) || squeezed($0.bundleName) == squeezed(stripped) },
        ]
        for test in tests {
            let hits = apps.filter(test)
            if let best = hits.sorted(by: { ($0.active ? 0 : 1, $0.pid) < ($1.active ? 0 : 1, $1.pid) }).first {
                return best
            }
        }
        let names = Set(apps.compactMap(\.name)).sorted().prefix(20)
        throw GuiError("no running app '\(query)' (running: \(names.joined(separator: ", ")))")
    }

    static func squeezed(_ s: String?) -> String? {
        s.map { $0.lowercased().filter { !" -_.".contains($0) } }
    }
}
