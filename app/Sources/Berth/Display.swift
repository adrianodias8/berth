import AppKit
import SwiftUI

// Display windows: one Linux GUI session's screen in a Mac window. For now
// an honest interim: a screenshot every couple of seconds through
// `berth gui screenshot --target linux:<machine> --session <s> --json`,
// watch-only. The GPU display (plan, section 2: berth-gpu scanouts as
// IOSurfaces in a CAMetalLayer, paced to the display link) replaces the
// frame source and the layer view below; the window, its toolbar and its
// value type stay. macOS VMs don't come here: their display is the VM
// process's own window (`berth mac start <vm> --display`).

/// Which screen a window shows. Codable: SwiftUI restores windows by value.
struct DisplayTarget: Codable, Hashable {
    /// "linux:<machine>" (docs/gui-protocol.md, Activity log).
    var target: String
    var session: String
}

/// How a Display window gets its frames. Only screenshots exist yet; the
/// Metal case arrives with berth-gpu (Phase 3).
enum DisplayBackend {
    case screenshots(every: Duration)
}

enum DisplayCapture {
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
        env["BERTH_GUI_WHO"] = "berth-app"
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
}

/// The latest frame of a session, refreshed while the window is open.
@MainActor
final class DisplayFrames: ObservableObject {
    @Published private(set) var image: CGImage?
    @Published private(set) var updated: Date?
    @Published private(set) var error: String?
    @Published var paused = false

    let target: DisplayTarget
    let backend: DisplayBackend

    init(target: DisplayTarget, backend: DisplayBackend = .screenshots(every: .seconds(2))) {
        self.target = target
        self.backend = backend
    }

    /// Runs until the window's task is cancelled.
    func run() async {
        switch backend {
        case let .screenshots(every):
            while !Task.isCancelled {
                if !paused { await capture() }
                try? await Task.sleep(for: every)
            }
        }
    }

    func capture() async {
        let args = DisplayCapture.arguments(target)
        let result = await offPool { DisplayCapture.run(args) }
        guard result.code == 0, let path = DisplayCapture.path(fromOutput: result.output) else {
            let message = result.output.trimmingCharacters(in: .whitespacesAndNewlines)
            error = message.isEmpty ? "No screenshot" : message
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
}

struct DisplayWindow: View {
    let target: DisplayTarget
    @StateObject private var frames: DisplayFrames

    init(target: DisplayTarget) {
        self.target = target
        _frames = StateObject(wrappedValue: DisplayFrames(target: target))
    }

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                Color.black
                if let image = frames.image {
                    FrameView(image: image)
                } else if let error = frames.error {
                    EmptyHint(icon: "display.trianglebadge.exclamationmark", text: error)
                } else {
                    ProgressView()
                }
            }
            Divider()
            HStack(spacing: 8) {
                Image(systemName: "eye").foregroundStyle(.secondary)
                Text("Watch-only · a screenshot every 2 s until the GPU display lands")
                Spacer()
                if let updated = frames.updated {
                    Text(frames.paused ? "Paused" : "Updated \(updated.formatted(date: .omitted, time: .standard))")
                }
                if let error = frames.error, frames.image != nil {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.yellow).help(error)
                }
            }
            .font(.caption).foregroundStyle(.secondary)
            .padding(.horizontal, 10).padding(.vertical, 5)
        }
        .navigationTitle("\(target.target) · \(target.session)")
        .toolbar {
            Button {
                frames.paused.toggle()
            } label: {
                Label(frames.paused ? "Resume" : "Pause", systemImage: frames.paused ? "play.fill" : "pause.fill")
            }
            .help(frames.paused ? "Resume refreshing" : "Pause refreshing")
            Button { Task { await frames.capture() } } label: { Label("Refresh", systemImage: "arrow.clockwise") }
                .help("Take a screenshot now")
        }
        .task { await frames.run() }
        .frame(minWidth: 480, minHeight: 320)
    }
}

/// A layer-backed view showing a frame scaled to fit. The GPU path swaps
/// this for a view whose layer is a CAMetalLayer fed by the session's
/// scanout; nothing else in the window changes.
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
