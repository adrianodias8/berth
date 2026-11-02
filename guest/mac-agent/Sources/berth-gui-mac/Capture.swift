// Screenshots (needs the Screen Recording grant). ScreenCaptureKit's
// SCScreenshotManager takes one frame of the main display or of one window at
// full pixel resolution; when it fails (an older guest, a TCC hiccup) the
// agent falls back to /usr/sbin/screencapture, which does the same through a
// separate tool. The completion-handler APIs are bridged with semaphores
// because each connection already runs on its own thread.
import AppKit
import CoreGraphics
import Foundation
import GuiCore
import ImageIO
import ScreenCaptureKit
import UniformTypeIdentifiers

enum Capture {
    static var allowed: Bool { CGPreflightScreenCaptureAccess() }

    struct Shot {
        var data: Data
        var width: Int
        var height: Int
    }

    /// Main display size in points, and its backing scale.
    static func screen() -> (CGSize, Double) {
        let id = CGMainDisplayID()
        let bounds = CGDisplayBounds(id)
        var scale = 1.0
        if let mode = CGDisplayCopyDisplayMode(id), mode.width > 0 {
            scale = Double(mode.pixelWidth) / Double(mode.width)
        }
        return (bounds.size, scale)
    }

    /// Waits for a completion handler; gives up after `timeout` seconds.
    static func waitFor<T>(_ timeout: Double = 10, _ start: (@escaping (T?, Error?) -> Void) -> Void) throws -> T {
        let sem = DispatchSemaphore(value: 0)
        var out: T?
        var err: Error?
        start { v, e in
            out = v
            err = e
            sem.signal()
        }
        if sem.wait(timeout: .now() + timeout) == .timedOut { throw GuiError("timed out waiting for ScreenCaptureKit") }
        if let out { return out }
        throw err.map { GuiError("\($0.localizedDescription)") } ?? GuiError("ScreenCaptureKit returned nothing")
    }

    static func content() throws -> SCShareableContent {
        try waitFor { done in
            SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { c, e in done(c, e) }
        }
    }

    static func image(_ filter: SCContentFilter, width: Int, height: Int) throws -> CGImage {
        let config = SCStreamConfiguration()
        config.width = max(1, width)
        config.height = max(1, height)
        config.showsCursor = false
        return try waitFor { done in
            SCScreenshotManager.captureImage(contentFilter: filter, configuration: config) { img, e in done(img, e) }
        }
    }

    /// The SCWindow to shoot for an app: its focused window if AX can tell,
    /// else its largest normal-layer window on screen.
    static func window(for app: NSRunningApplication, in content: SCShareableContent) throws -> SCWindow {
        let mine = content.windows.filter {
            $0.owningApplication?.processID == app.processIdentifier && $0.windowLayer == 0 && $0.frame.width > 1
        }
        guard !mine.isEmpty else { throw GuiError("\(app.localizedName ?? "app") has no window on screen") }
        if AX.trusted, let w = AX.element(AX.app(app.processIdentifier), kAXFocusedWindowAttribute),
           let f = AX.frame(w),
           let match = mine.first(where: { abs($0.frame.origin.x - f.x) < 2 && abs($0.frame.origin.y - f.y) < 2 }) {
            return match
        }
        return mine.max { $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height }!
    }

    static func shoot(app: NSRunningApplication?, format: String) throws -> Shot {
        guard allowed else {
            throw GuiError("Screen Recording is not granted to berth-gui-mac (System Settings > Privacy & Security > Screen & System Audio Recording)")
        }
        let type = try imageType(format)
        do {
            let c = try content()
            let img: CGImage
            if let app {
                let w = try window(for: app, in: c)
                let filter = SCContentFilter(desktopIndependentWindow: w)
                let scale = Double(filter.pointPixelScale)
                img = try image(filter, width: Int(filter.contentRect.width * scale), height: Int(filter.contentRect.height * scale))
            } else {
                guard let d = c.displays.first(where: { $0.displayID == CGMainDisplayID() }) ?? c.displays.first else {
                    throw GuiError("no display")
                }
                let (_, scale) = screen()
                let filter = SCContentFilter(display: d, excludingWindows: [])
                img = try image(filter, width: Int(Double(d.width) * scale), height: Int(Double(d.height) * scale))
            }
            return Shot(data: try encode(img, type: type), width: img.width, height: img.height)
        } catch let e as GuiError where e.message.hasSuffix("has no window on screen") {
            throw e
        } catch {
            log("ScreenCaptureKit failed (\(error)); trying screencapture")
            return try screencapture(app: app, format: format)
        }
    }

    static func imageType(_ format: String) throws -> UTType {
        switch format.lowercased() {
        case "png": return .png
        case "jpg", "jpeg": return .jpeg
        default: throw GuiError("unsupported screenshot format '\(format)' (png or jpeg)")
        }
    }

    static func encode(_ img: CGImage, type: UTType) throws -> Data {
        let out = NSMutableData()
        guard let dest = CGImageDestinationCreateWithData(out, type.identifier as CFString, 1, nil) else {
            throw GuiError("can't encode \(type.identifier)")
        }
        CGImageDestinationAddImage(dest, img, nil)
        guard CGImageDestinationFinalize(dest) else { throw GuiError("can't encode \(type.identifier)") }
        return out as Data
    }

    /// /usr/sbin/screencapture into a temporary file; `-l` picks a window by
    /// its CGWindowID.
    static func screencapture(app: NSRunningApplication?, format: String) throws -> Shot {
        let ext = format.lowercased() == "png" ? "png" : "jpg"
        let file = FileManager.default.temporaryDirectory
            .appendingPathComponent("berth-gui-\(UUID().uuidString).\(ext)")
        defer { try? FileManager.default.removeItem(at: file) }
        var args = ["-x", "-t", ext]
        if let app {
            guard let id = Apps.windowID(pid: app.processIdentifier) else {
                throw GuiError("\(app.localizedName ?? "app") has no window on screen")
            }
            args += ["-o", "-l", String(id)]
        }
        args.append(file.path)
        let (status, err) = run("/usr/sbin/screencapture", args)
        guard status == 0, let data = try? Data(contentsOf: file), !data.isEmpty else {
            throw GuiError("screencapture failed: \(err.isEmpty ? "exit \(status)" : err)")
        }
        guard let src = CGImageSourceCreateWithData(data as CFData, nil),
              let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any] else {
            throw GuiError("screencapture wrote an unreadable image")
        }
        let w = props[kCGImagePropertyPixelWidth] as? Int ?? 0
        let h = props[kCGImagePropertyPixelHeight] as? Int ?? 0
        return Shot(data: data, width: w, height: h)
    }

    /// Thumbnail of the main display as RGBA bytes at a fixed size, for
    /// screen-idle detection. Small so a frame costs milliseconds.
    static func thumbnail(width: Int = 192, height: Int = 120) throws -> [UInt8] {
        guard allowed else { throw GuiError("waiting for an idle screen needs Screen Recording") }
        let c = try content()
        guard let d = c.displays.first(where: { $0.displayID == CGMainDisplayID() }) ?? c.displays.first else {
            throw GuiError("no display")
        }
        let img = try image(SCContentFilter(display: d, excludingWindows: []), width: width, height: height)
        var bytes = [UInt8](repeating: 0, count: width * height * 4)
        let ok = bytes.withUnsafeMutableBytes { raw -> Bool in
            guard let ctx = CGContext(data: raw.baseAddress, width: width, height: height, bitsPerComponent: 8,
                                      bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
                                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return false }
            ctx.draw(img, in: CGRect(x: 0, y: 0, width: width, height: height))
            return true
        }
        guard ok else { throw GuiError("can't read the screen thumbnail") }
        return bytes
    }
}

/// Runs a tool to completion; returns its status and stderr.
@discardableResult
func run(_ path: String, _ args: [String], stdin: FileHandle? = nil) -> (Int32, String) {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: path)
    p.arguments = args
    let err = Pipe()
    p.standardError = err
    p.standardOutput = FileHandle.nullDevice
    if let stdin { p.standardInput = stdin }
    do { try p.run() } catch { return (-1, "\(error)") }
    let data = err.fileHandleForReading.readDataToEndOfFile()
    p.waitUntilExit()
    return (p.terminationStatus, String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines))
}
