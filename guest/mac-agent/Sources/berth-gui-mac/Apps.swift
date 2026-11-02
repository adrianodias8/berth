// Apps and windows: open, focus, quit, list. NSWorkspace launches apps and
// NSRunningApplication tracks them (its lists are refreshed on the main run
// loop, which main.swift keeps running). Window titles and focus come from AX
// when it's granted; without it, CGWindowList still gives owners and frames.
import AppKit
import ApplicationServices
import Foundation
import GuiCore

enum Apps {
    static func running() -> [NSRunningApplication] {
        NSWorkspace.shared.runningApplications.filter { !$0.isTerminated }
    }

    static func info(_ a: NSRunningApplication) -> AppInfo {
        AppInfo(pid: a.processIdentifier, name: a.localizedName, bundleID: a.bundleIdentifier,
                path: a.bundleURL?.path, active: a.isActive)
    }

    static func find(_ query: String) throws -> NSRunningApplication {
        let apps = running()
        let match = try AppMatch.find(query, in: apps.map(info))
        guard let a = apps.first(where: { $0.processIdentifier == match.pid }) else {
            throw GuiError("no running app '\(query)'")
        }
        return a
    }

    /// The app named by `args.app`, or the frontmost one when it's absent.
    static func target(_ name: String?) throws -> NSRunningApplication {
        if let name, !name.isEmpty { return try find(name) }
        guard let front = NSWorkspace.shared.frontmostApplication else { throw GuiError("no frontmost app") }
        return front
    }

    static let searchFolders: [String] = {
        let home = NSHomeDirectory()
        return [
            "\(home)/Pushed", "/Applications", "\(home)/Applications", "/System/Applications",
            "/System/Applications/Utilities", "/Applications/Utilities",
        ]
    }()

    /// What `open` was given, as an app bundle: a path, a bundle id, or a
    /// name looked up in the usual folders (pushed apps first).
    static func resolve(_ app: String) throws -> URL {
        let fm = FileManager.default
        let expanded = (app as NSString).expandingTildeInPath
        if expanded.hasPrefix("/") {
            guard fm.fileExists(atPath: expanded) else { throw GuiError("open: no such app \(expanded)") }
            return URL(fileURLWithPath: expanded)
        }
        if !app.contains("/"), app.contains("."), !app.lowercased().hasSuffix(".app"),
           let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: app) {
            return url
        }
        let want = (app.lowercased().hasSuffix(".app") ? app : app + ".app").lowercased()
        for folder in searchFolders {
            guard let names = try? fm.contentsOfDirectory(atPath: folder) else { continue }
            if let hit = names.first(where: { $0.lowercased() == want }) {
                return URL(fileURLWithPath: folder).appendingPathComponent(hit)
            }
        }
        throw GuiError("open: no app '\(app)' in \(searchFolders.joined(separator: ", "))")
    }

    /// Launches (or re-activates) an app and waits for its first window.
    static func open(_ app: String, args: [String], wait: Double) throws -> (pid_t, String?, String) {
        let url = try resolve(app)
        let config = NSWorkspace.OpenConfiguration()
        config.arguments = args
        config.activates = true
        config.promptsUserIfNeeded = false
        let sem = DispatchSemaphore(value: 0)
        var launched: NSRunningApplication?
        var failure: Error?
        NSWorkspace.shared.openApplication(at: url, configuration: config) { a, e in
            launched = a
            failure = e
            sem.signal()
        }
        if sem.wait(timeout: .now() + max(wait, 5)) == .timedOut {
            throw GuiError("open: \(url.lastPathComponent) didn't launch within \(Int(max(wait, 5))) s")
        }
        guard let a = launched else {
            throw GuiError("open: \(failure?.localizedDescription ?? "launch failed")")
        }
        let name = a.localizedName ?? url.deletingPathExtension().lastPathComponent
        let deadline = Date().addingTimeInterval(wait)
        while true {
            if let title = firstWindow(a.processIdentifier) { return (a.processIdentifier, title, name) }
            if a.isTerminated { throw GuiError("open: \(name) quit before showing a window") }
            if Date() >= deadline { throw GuiError("open: \(name) (pid \(a.processIdentifier)) showed no window within \(Int(wait)) s") }
            usleep(200_000)
        }
    }

    /// The title of a window the app shows, or nil if it has none yet. AX
    /// knows titles; CGWindowList (no AX grant) only that a window exists, so
    /// then the app's name stands in.
    static func firstWindow(_ pid: pid_t) -> String? {
        if AX.trusted {
            let wins = AX.elements(AX.app(pid), kAXWindowsAttribute)
            if let w = wins.first { return AX.string(w, kAXTitleAttribute) ?? "" }
            return nil
        }
        guard windowID(pid: pid) != nil else { return nil }
        return NSRunningApplication(processIdentifier: pid)?.localizedName ?? ""
    }

    /// The frontmost normal window of a process, from CGWindowList.
    static func windowID(pid: pid_t) -> CGWindowID? {
        let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
        for w in list {
            guard (w[kCGWindowOwnerPID as String] as? Int).map(pid_t.init) == pid,
                  (w[kCGWindowLayer as String] as? Int) == 0,
                  let id = w[kCGWindowNumber as String] as? Int else { continue }
            return CGWindowID(id)
        }
        return nil
    }

    static func windows() -> JSON {
        var out: [JSON] = []
        if AX.trusted {
            for a in running() where a.activationPolicy == .regular {
                let el = AX.app(a.processIdentifier)
                let focusedWin = AX.element(el, kAXFocusedWindowAttribute)
                for w in AX.elements(el, kAXWindowsAttribute) {
                    let focused = a.isActive && focusedWin.map { CFEqual($0, w) } == true
                    out.append(.object([
                        "app": .from(a.localizedName),
                        "title": .from(AX.string(w, kAXTitleAttribute)),
                        "frame": AX.frame(w)?.json ?? .null,
                        "focused": .bool(focused),
                    ]))
                }
            }
        } else {
            let front = NSWorkspace.shared.frontmostApplication?.processIdentifier
            var seenFront = false
            let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
            for w in list where (w[kCGWindowLayer as String] as? Int) == 0 {
                let pid = (w[kCGWindowOwnerPID as String] as? Int).map(pid_t.init)
                var frame: JSON = .null
                if let b = w[kCGWindowBounds as String] as? NSDictionary,
                   let r = CGRect(dictionaryRepresentation: b) {
                    frame = .frame(r.origin.x, r.origin.y, r.width, r.height)
                }
                // The list is front to back, so the front app's first window
                // is its focused one.
                let focused = pid == front && !seenFront
                if focused { seenFront = true }
                out.append(.object([
                    "app": .from(w[kCGWindowOwnerName as String] as? String),
                    // Titles need Screen Recording; null without it.
                    "title": .from(w[kCGWindowName as String] as? String),
                    "frame": frame,
                    "focused": .bool(focused),
                ]))
            }
        }
        return .object(["windows": .array(out)])
    }

    static func focus(_ query: String) throws {
        let a = try find(query)
        a.unhide()
        // Since macOS 14 activation is cooperative and a background process's
        // request can be ignored; AX's "frontmost" is honoured, so set both.
        a.activate()
        if AX.trusted {
            let el = AX.app(a.processIdentifier)
            AXUIElementSetAttributeValue(el, kAXFrontmostAttribute as CFString, kCFBooleanTrue)
            if let w = AX.element(el, kAXMainWindowAttribute) ?? AX.elements(el, kAXWindowsAttribute).first {
                AXUIElementPerformAction(w, kAXRaiseAction as CFString)
            }
        }
        let deadline = Date().addingTimeInterval(3)
        while NSWorkspace.shared.frontmostApplication?.processIdentifier != a.processIdentifier {
            if Date() >= deadline { throw GuiError("focus: \(a.localizedName ?? query) didn't come to the front") }
            usleep(100_000)
        }
    }

    static func quit(_ query: String) throws {
        let a = try find(query)
        guard a.terminate() else { throw GuiError("quit: \(a.localizedName ?? query) refused to quit") }
        let deadline = Date().addingTimeInterval(10)
        while !a.isTerminated {
            if Date() >= deadline {
                throw GuiError("quit: \(a.localizedName ?? query) is still running after 10 s (an unsaved-changes sheet?)")
            }
            usleep(100_000)
        }
    }
}
