import AppKit
import SwiftUI

// Display windows: one Linux GUI session's screen in a Mac window. Live
// when the daemon streams the session's surface (SurfaceStream.swift,
// docs/display-protocol.md): frames from a shared-memory ring in a
// CAMetalLayer paced to the display link (MetalFrameView.swift), and with
// Take control the Mac's keyboard and mouse drive the guest
// (GuestInput.swift). A daemon without display streams gets the interim:
// a screenshot every couple of seconds through `berth gui screenshot
// --target linux:<machine> --session <s> --json`, watch-only. macOS VMs
// don't come here: their display is the VM process's own window
// (`berth mac start <vm> --display`).

/// Which screen a window shows. Codable: SwiftUI restores windows by value.
struct DisplayTarget: Codable, Hashable {
    /// "linux:<machine>" (docs/gui-protocol.md, Activity log); the Agents
    /// view's cards also shoot "mac:<vm>" sessions this way.
    var target: String
    var session: String
    /// A desktop session's scanout (it owns card0), from the session JSON;
    /// nil for headless sessions, which stream their own surface.
    var scanout: Int? = nil
    /// "gles2" or "pixman": how the session's compositor renders.
    var renderer: String? = nil

    var isLinux: Bool { target.hasPrefix("linux:") }
}

/// How a Display window gets its frames.
enum DisplayBackend: Equatable {
    /// The daemon's display stream; screenshots when it has none.
    case stream(maxFPS: Int)
    case screenshots(every: Duration)

    static let fallback = DisplayBackend.screenshots(every: .seconds(2))

    static func preferred(for target: DisplayTarget) -> DisplayBackend {
        target.isLinux ? .stream(maxFPS: 60) : fallback
    }
}

enum DisplayCapture {
    /// BERTH_GUI_WHO for the app's own screenshots, and the `who.kind` the
    /// CLI gives them (docs/gui-protocol.md, Activity log).
    static let who = "berth-app"

    /// The PNG's path in `berth gui screenshot --json` output: the "path"
    /// (or "file"/"screenshot") of the last JSON object printed, else a
    /// last line that is itself a path.
    static func path(fromOutput output: String) -> String? {
        let lines = output.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }
        for line in lines.reversed() where line.hasPrefix("{") {
            guard let object = (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any] else { continue }
            for key in ["path", "file", "screenshot"] {
                if let path = object[key] as? String, !path.isEmpty { return path }
            }
            if let result = object["result"] as? [String: Any], let path = result["path"] as? String { return path }
        }
        if let last = lines.last(where: { !$0.isEmpty }), last.hasPrefix("/"), last.lowercased().hasSuffix(".png") { return last }
        return nil
    }

    /// Engine.run with BERTH_GUI_WHO=berth-app added, so the activity log
    /// tells the app's refreshes from an agent's commands
    /// (docs/gui-protocol.md, Activity log) and the Agents view hides them.
    static func run(_ args: [String]) -> (code: Int32, output: String) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: BerthPaths.cli)
        process.arguments = args
        var env = ProcessInfo.processInfo.environment
        env["BERTH_GUI_WHO"] = who
        process.environment = env
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        do {
            try process.run()
        } catch {
            return (-1, "could not run berth: \(error.localizedDescription)")
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return (process.terminationStatus, String(decoding: data, as: UTF8.self))
    }

    static func arguments(_ target: DisplayTarget) -> [String] {
        ["gui", "screenshot", "--target", target.target, "--session", target.session, "--json"]
    }

    struct Failure: Error {
        let message: String
    }

    /// One screenshot of a session: the PNG's path (for the app the CLI
    /// replaces one file per session, `shots/live-<session>.png`), or what
    /// went wrong. Display windows and the Agents view's cards both use it.
    static func shoot(_ target: DisplayTarget) async -> Result<String, Failure> {
        let args = arguments(target)
        let result = await offPool { run(args) }
        guard result.code == 0, let path = path(fromOutput: result.output) else {
            let message = result.output.trimmingCharacters(in: .whitespacesAndNewlines)
            return .failure(Failure(message: message.isEmpty ? "No screenshot" : message))
        }
        return .success(path)
    }
}


/// A session's screen while its window is open: a live stream, or the
/// newest screenshot when the daemon has no stream for it.
@MainActor
final class DisplayFrames: ObservableObject {
    enum Mode: Equatable {
        case connecting
        case live
        /// Screenshots, with why the stream isn't there.
        case screenshots(reason: String?)
    }

    @Published private(set) var mode: Mode
    @Published private(set) var image: CGImage?
    @Published private(set) var updated: Date?
    @Published private(set) var error: String?
    /// New frames shown in the last second (live only).
    @Published var fps: Int?
    @Published var paused = false
    /// Mac input goes to the guest (live only).
    @Published var controlling = false
    @Published private(set) var stream: SurfaceStream?

    let target: DisplayTarget
    let backend: DisplayBackend

    init(target: DisplayTarget, backend: DisplayBackend? = nil) {
        self.target = target
        self.backend = backend ?? .preferred(for: target)
        if case .screenshots = self.backend { mode = .screenshots(reason: nil) } else { mode = .connecting }
    }

    /// Runs until the window's task is cancelled.
    func run() async {
        var every: Duration
        switch backend {
        case let .screenshots(interval):
            every = interval
        case let .stream(maxFPS):
            let reason = await streamUntilUnavailable(maxFPS: maxFPS)
            guard !Task.isCancelled else { return }
            controlling = false
            fps = nil
            mode = .screenshots(reason: reason)
            guard case let .screenshots(interval) = DisplayBackend.fallback else { return }
            every = interval
        }
        while !Task.isCancelled {
            if !paused { await capture() }
            try? await Task.sleep(for: every)
        }
    }

    /// Streams until cancelled (nil) or until the stream gives up (why).
    private func streamUntilUnavailable(maxFPS: Int) async -> String? {
        let stream = SurfaceStream(target: target, maxFPS: maxFPS)
        let (states, continuation) = AsyncStream.makeStream(of: SurfaceStream.State.self)
        stream.onState = { continuation.yield($0) }
        self.stream = stream
        stream.start()
        defer {
            stream.cancel()
            self.stream = nil
        }
        // Ends when the task is cancelled.
        for await state in states {
            switch state {
            case .connecting:
                if mode != .connecting, image == nil { mode = .connecting }
            case .live:
                mode = .live
                error = nil
            case let .unavailable(reason):
                return reason
            }
        }
        return nil
    }

    /// Refresh: a new screenshot, or a fresh connection when live.
    func refresh() async {
        if let stream {
            stream.reconnect()
        } else {
            await capture()
        }
    }

    func capture() async {
        let path: String
        switch await DisplayCapture.shoot(target) {
        case let .success(saved): path = saved
        case let .failure(failure):
            error = failure.message
            return
        }
        let image = await offPool { () -> CGImage? in
            guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil) else { return nil }
            return CGImageSourceCreateImageAtIndex(source, 0, nil)
        }
        if let image {
            self.image = image
            updated = Date()
            error = nil
        } else {
            error = "Couldn't read \(path)"
        }
    }

    /// "GPU" or "CPU", from how the session renders.
    var rendererName: String? {
        switch target.renderer {
        case "gles2": return "GPU"
        case "pixman": return "CPU"
        default: return nil
        }
    }
}

struct DisplayWindow: View {
    let target: DisplayTarget
    @StateObject private var frames: DisplayFrames

    init(target: DisplayTarget) {
        self.target = target
        _frames = StateObject(wrappedValue: DisplayFrames(target: target))
    }

    private var isLive: Bool { frames.mode == .live }

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                Color.black
                switch frames.mode {
                case .live, .connecting:
                    if let stream = frames.stream {
                        LiveFrameView(stream: stream, paused: frames.paused, controlling: $frames.controlling,
                                      fps: $frames.fps)
                    }
                    if frames.mode == .connecting { ProgressView() }
                case .screenshots:
                    if let image = frames.image {
                        FrameView(image: image)
                    } else if let error = frames.error {
                        EmptyHint(icon: "display.trianglebadge.exclamationmark", text: error)
                    } else {
                        ProgressView()
                    }
                }
            }
            Divider()
            status
                .font(.caption).foregroundStyle(.secondary)
                .padding(.horizontal, 10).padding(.vertical, 5)
        }
        .navigationTitle("\(target.target) · \(target.session)")
        .toolbar {
            Toggle(isOn: $frames.controlling) {
                Label("Take Control", systemImage: frames.controlling ? "hand.point.up.left.fill" : "hand.point.up.left")
            }
            .disabled(!isLive)
            .help(isLive
                ? "Send this Mac's keyboard and mouse to the session. ⌃⌥↩ gives them back."
                : "Needs the live display; this berth sends screenshots only")
            Button {
                frames.paused.toggle()
            } label: {
                Label(frames.paused ? "Resume" : "Pause", systemImage: frames.paused ? "play.fill" : "pause.fill")
            }
            .help(frames.paused ? "Resume refreshing" : "Pause refreshing")
            Button { Task { await frames.refresh() } } label: { Label("Refresh", systemImage: "arrow.clockwise") }
                .help(isLive ? "Reconnect to the display" : "Take a screenshot now")
        }
        .task { await frames.run() }
        .frame(minWidth: 480, minHeight: 320)
    }

    @ViewBuilder private var status: some View {
        HStack(spacing: 8) {
            switch frames.mode {
            case .live:
                Image(systemName: frames.controlling ? "keyboard" : "eye").foregroundStyle(.secondary)
                Text(frames.controlling ? "In control · ⌃⌥↩ to release" : "Live · watching")
                Spacer()
                if frames.paused {
                    Text("Paused")
                } else {
                    Text([frames.fps.map { "\($0) fps" }, frames.rendererName].compactMap { $0 }.joined(separator: " · "))
                        .monospacedDigit()
                }
            case .connecting:
                Image(systemName: "antenna.radiowaves.left.and.right").foregroundStyle(.secondary)
                Text("Connecting to the display…")
                Spacer()
            case let .screenshots(reason):
                Image(systemName: "eye").foregroundStyle(.secondary)
                Text("Watch-only · a screenshot every 2 s")
                    .help(reason.map { "No live display: \($0)" } ?? "")
                Spacer()
                if let updated = frames.updated {
                    Text(frames.paused ? "Paused" : "Updated \(updated.formatted(date: .omitted, time: .standard))")
                }
                if let error = frames.error, frames.image != nil {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.yellow).help(error)
                }
            }
        }
    }
}

/// The live surface: a MetalFrameView fed by the window's SurfaceStream.
struct LiveFrameView: NSViewRepresentable {
    let stream: SurfaceStream
    let paused: Bool
    @Binding var controlling: Bool
    @Binding var fps: Int?

    func makeNSView(context: Context) -> MetalFrameView {
        let view = MetalFrameView(frame: .zero)
        view.onControlChange = { on in DispatchQueue.main.async { controlling = on } }
        view.onFPS = { value in fps = value }
        return view
    }

    func updateNSView(_ view: MetalFrameView, context: Context) {
        view.stream = stream
        view.paused = paused
        view.onControlChange = { on in DispatchQueue.main.async { controlling = on } }
        view.onFPS = { value in fps = value }
        view.setControlling(controlling)
    }

    static func dismantleNSView(_ view: MetalFrameView, coordinator: ()) {
        view.setControlling(false)
    }
}

/// A layer-backed view showing a screenshot scaled to fit: the fallback
/// when the daemon has no display stream.
struct FrameView: NSViewRepresentable {
    let image: CGImage

    func makeNSView(context: Context) -> FrameLayerView { FrameLayerView() }

    func updateNSView(_ view: FrameLayerView, context: Context) { view.show(image) }

    final class FrameLayerView: NSView {
        override init(frame: NSRect) {
            super.init(frame: frame)
            wantsLayer = true
            layer?.contentsGravity = .resizeAspect
            layer?.backgroundColor = NSColor.black.cgColor
            // Text in a guest's screen stays crisp when scaled down.
            layer?.minificationFilter = .trilinear
        }

        required init?(coder: NSCoder) { fatalError("not used") }

        func show(_ image: CGImage) {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            layer?.contents = image
            CATransaction.commit()
        }
    }
}
