// Where `push` puts things. The name comes from the host and becomes one
// entry under the destination folder (~/Pushed by default), so it must be a
// single path component: no "/", no "..", nothing hidden behind a NUL.
import Foundation

public enum PushPath {
    public static func validateName(_ name: String) throws -> String {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !n.isEmpty else { throw GuiError("push: empty name") }
        guard n != ".", n != "..", !n.contains("/"), !n.contains("\0") else {
            throw GuiError("push: name must be a single file or folder name, got '\(name)'")
        }
        return n
    }

    /// The folder to push into: `dest` if given ("~" means home, relative
    /// paths are under home), else ~/Pushed.
    public static func destination(_ dest: String?, home: String) -> String {
        guard let d = dest?.trimmingCharacters(in: .whitespaces), !d.isEmpty else {
            return (home as NSString).appendingPathComponent("Pushed")
        }
        if d == "~" { return home }
        if d.hasPrefix("~/") { return (home as NSString).appendingPathComponent(String(d.dropFirst(2))) }
        if d.hasPrefix("/") { return (d as NSString).standardizingPath }
        return ((home as NSString).appendingPathComponent(d) as NSString).standardizingPath
    }
}
