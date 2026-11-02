import AppKit
import SwiftUI

// macOS VMs: the list from `berth mac list --json` and its actions (create,
// start, start with a display, stop, reset, clone, make a base image, delete,
// info), all through the CLI, which owns ~/.berth/mac (docs/gui-protocol.md,
// Transport). Hidden unless ~/.berth/config.json has "mac": true. Also the
// feature flags the GUI sections share, read here until BerthConfig knows them.

// MARK: - Feature flags

/// The "mac" and "gui" switches in ~/.berth/config.json. Off unless set to
/// true, so without them the app is exactly as before. Re-read every few
/// seconds so `berth config set mac true` shows the sections without a
/// relaunch (reading a small file; the VM isn't involved).
@MainActor
final class FeatureFlags: ObservableObject {
    static let shared = FeatureFlags(config: BerthPaths.config)

    @Published private(set) var mac = false
    @Published private(set) var gui = false
    /// The Agents section covers both macOS and Linux sessions.
    var agents: Bool { mac || gui }

    private let config: URL
    private var timer: Timer?

    init(config: URL, watch: Bool = true) {
        self.config = config
        reload()
        if watch {
            timer = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] _ in
                Task { @MainActor in self?.reload() }
            }
        }
    }

    func reload() {
        let flags = Self.read(config)
        if flags.mac != mac { mac = flags.mac }
        if flags.gui != gui { gui = flags.gui }
    }

    /// Only a JSON `true` (or the string "true") turns a flag on: a 1 that
    /// JSONSerialization would bridge to Bool doesn't.
    nonisolated static func read(_ url: URL) -> (mac: Bool, gui: Bool) {
        guard let data = try? Data(contentsOf: url),
              let dict = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { return (false, false) }
        func on(_ key: String) -> Bool {
            if let number = dict[key] as? NSNumber, CFGetTypeID(number) == CFBooleanGetTypeID() { return number.boolValue }
            return (dict[key] as? String)?.lowercased() == "true"
        }
        return (on("mac"), on("gui"))
    }
}

// MARK: - Model and CLI

/// One entry of `berth mac list --json`.
struct MacVM: Decodable, Identifiable, Hashable {
    let name: String
    let state: String
    /// The VM this one was cloned from, if any.
    let base: String?
    let cpus: Int
    let memoryMib: Int
    let diskGib: Int
    let ip: String?
    var id: String { name }
    var isRunning: Bool { state == "running" }

    enum CodingKeys: String, CodingKey {
        case name, state, base, cpus, ip
        case memoryMib = "memory_mib"
        case diskGib = "disk_gib"
    }
}

/// `berth mac …` through the CLI: each VM is a process of its own that the
/// CLI starts and talks to, not something the daemon's socket serves.
enum MacVMs {
    /// Apple's license: two macOS VMs running at once.
    static let runningLimit = 2

    static func list() async throws -> [MacVM] {
        let result = await offPool { Engine.run([BerthPaths.cli, "mac", "list", "--json"]) }
        guard result.code == 0 else {
            throw SocketError.message(result.output.trimmingCharacters(in: .whitespacesAndNewlines))
        }
        return try JSONDecoder().decode([MacVM].self, from: Data(result.output.utf8))
    }

    /// Run `berth mac …` off the main thread; the error output if it failed.
    static func run(_ args: [String]) async -> String? {
        let result = await offPool { Engine.run([BerthPaths.cli, "mac"] + args) }
        return result.code == 0 ? nil : result.output.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// `berth mac create`'s arguments; without a file it downloads the
    /// latest IPSW this Mac supports.
    static func createArgs(_ new: NewMacVM) -> [String] {
        var args = ["create", new.name, "--cpus", String(new.cpus), "--memory", String(new.memoryMib), "--disk", String(new.diskGib)]
        if let ipsw = new.ipsw { args += ["--ipsw", ipsw.path] }
        return args
    }

    /// Lowercase letters, digits and -, 1–32 characters: it names a folder
    /// in ~/.berth/mac and the guest's host name.
    static func validName(_ name: String) -> Bool {
        (1...32).contains(name.count) && !name.hasPrefix("-")
            && name.allSatisfy { $0.isASCII && ($0.isLowercase || $0.isNumber || $0 == "-") }
    }

    /// "8 GiB", "1.5 GiB"; formatted by hand (LEARNINGS: locale formatting).
    static func gib(mib: Int) -> String {
        mib % 1024 == 0 ? "\(mib / 1024) GiB" : String(format: "%.1f GiB", Double(mib) / 1024)
    }
}

struct NewMacVM {
    var name: String
    var cpus: Int
    var memoryMib: Int
    var diskGib: Int
    /// A restore image chosen by the user; nil downloads the latest.
    var ipsw: URL?
}

// MARK: - Creating (minutes to an hour: an IPSW is about 15 GB)

/// Terminal output as a person would see it: a carriage return starts the
/// line over (download progress bars) and ANSI escapes are dropped. Kept as
/// lines so an hour of progress updates doesn't grow without bound.
struct ConsoleText: Equatable {
    private(set) var lines: [String] = []
    private(set) var current = ""
    private var returned = false
    private var escape = Escape.none
    private enum Escape { case none, started, csi }
    var maxLines = 2000

    mutating func append(_ chunk: String) {
        for scalar in chunk.unicodeScalars {
            switch escape {
            case .started:
                escape = scalar == "[" ? .csi : .none
                continue
            case .csi:
                // A CSI sequence ends with a byte in @…~.
                if (0x40...0x7e).contains(scalar.value) { escape = .none }
                continue
            case .none:
                break
            }
            switch scalar {
            case "\u{1b}":
                escape = .started
            case "\n":
                lines.append(current)
                current = ""
                returned = false
                if lines.count > maxLines { lines.removeFirst(lines.count - maxLines) }
            case "\r":
                returned = true
            default:
                if returned { current = ""; returned = false }
                current.unicodeScalars.append(scalar)
            }
        }
    }

    var text: String { (current.isEmpty ? lines : lines + [current]).joined(separator: "\n") }

    /// The latest "NN%" in the last few lines, as 0…1, for a progress bar.
    var fraction: Double? {
        for line in ([current] + lines.suffix(5).reversed()) {
            if let p = Self.percent(in: line) { return p }
        }
        return nil
    }

    static func percent(in line: String) -> Double? {
        let regex = try! NSRegularExpression(pattern: #"(\d{1,3}(?:\.\d+)?)\s?%"#)
        let ns = line as NSString
        guard let match = regex.matches(in: line, range: NSRange(location: 0, length: ns.length)).last,
              let value = Double(ns.substring(with: match.range(at: 1))), value <= 100 else { return nil }
        return value / 100
    }
}

/// One `berth mac create`, kept outside the sheet so closing the sheet (or
/// leaving the screen) doesn't stop it.
@MainActor
final class MacCreateJob: ObservableObject, Identifiable {
    let name: String
    @Published private(set) var console = ConsoleText()
    @Published private(set) var running = false
    @Published private(set) var exitCode: Int32?
    private var process: Process?

    init(name: String) { self.name = name }

    var failed: Bool { exitCode.map { $0 != 0 } ?? false }

    func start(_ args: [String], done: @escaping @MainActor (MacCreateJob) -> Void) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: BerthPaths.cli)
        p.arguments = ["mac"] + args
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let text = String(decoding: handle.availableData, as: UTF8.self)
            if !text.isEmpty { Task { @MainActor in self?.console.append(text) } }
        }
        p.terminationHandler = { [weak self] finished in
            let code = finished.terminationStatus
            Task { @MainActor in
                pipe.fileHandleForReading.readabilityHandler = nil
                if let rest = try? pipe.fileHandleForReading.readToEnd() { self?.console.append(String(decoding: rest, as: UTF8.self)) }
                guard let self else { return }
                self.exitCode = code
                self.running = false
                self.process = nil
                done(self)
            }
        }
        do {
            try p.run()
            process = p
            running = true
        } catch {
            console.append("could not run berth: \(error.localizedDescription)\n")
            exitCode = -1
            done(self)
        }
    }

    func stop() { process?.terminate() }
}

@MainActor
final class MacCreateJobs: ObservableObject {
    static let shared = MacCreateJobs()
    @Published private(set) var jobs: [MacCreateJob] = []
    /// Bumped when a create succeeds, so the list reloads at once.
    @Published private(set) var finished = 0

    func start(_ new: NewMacVM) -> MacCreateJob {
        let job = MacCreateJob(name: new.name)
        jobs.append(job)
        job.start(MacVMs.createArgs(new)) { [weak self] job in
            guard let self else { return }
            // A failed one stays listed with its output until dismissed.
            if !job.failed { self.jobs.removeAll { $0 === job } }
            self.finished += 1
        }
        return job
    }

    func dismiss(_ job: MacCreateJob) { jobs.removeAll { $0 === job } }
}

// MARK: - Views

struct MacVMsView: View {
    @EnvironmentObject var engine: Engine
    @ObservedObject private var creates = MacCreateJobs.shared
    @State private var vms: [MacVM] = []
    @State private var loadError: String?
    @State private var loaded = false
    // `-openMacVM test` picks one; `-openSheet NewMacVM` opens the sheet.
    @State private var selection: String? = UserDefaults.standard.string(forKey: "openMacVM")
    @State private var sheet: MacSheet?
    @State private var busy: Set<String> = []
    @State private var confirmDelete: MacVM?
    @State private var confirmReset: MacVM?
    @State private var cloning: MacVM?
    @State private var cloneName = ""
    @State private var report: MachineReport?

    private var runningCount: Int { vms.filter(\.isRunning).count }

    var body: some View {
        HStack(spacing: 0) {
            List(selection: $selection) {
                ForEach(vms) { vm in
                    HStack {
                        StatusDot(running: vm.isRunning, size: 7)
                        VStack(alignment: .leading, spacing: 1) {
                            Text(vm.name).fontWeight(.medium)
                            Text(vm.base.map { "Clone of \($0)" } ?? "\(vm.cpus) CPUs · \(MacVMs.gib(mib: vm.memoryMib))")
                                .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                        }
                        Spacer()
                        if busy.contains(vm.name) { ProgressView().controlSize(.small) }
                    }
                    .tag(vm.name)
                    .contextMenu { menu(vm) }
                }
                ForEach(creates.jobs) { job in
                    CreateJobRow(job: job) { sheet = MacSheet(job: job) }
                }
            }
            .overlay {
                if loaded && vms.isEmpty && creates.jobs.isEmpty {
                    if let loadError {
                        ContentUnavailableView("Can't List macOS VMs", systemImage: "exclamationmark.triangle",
                                               description: Text(loadError))
                    } else {
                        ContentUnavailableView {
                            Label("No macOS VMs", systemImage: "macwindow")
                        } description: {
                            Text("A clean macOS to test Mac apps in. Make one a base image and its clones reset to clean in about a second.")
                        } actions: {
                            Button("New macOS VM…") { sheet = MacSheet(job: nil) }
                        }
                    }
                }
            }
            .safeAreaInset(edge: .bottom) {
                if !vms.isEmpty {
                    Text("\(runningCount) of \(MacVMs.runningLimit) running (Apple's limit)")
                        .font(.caption).foregroundStyle(.secondary).padding(8)
                }
            }
            .frame(width: 260)
            Divider()
            Group {
                if let name = selection, let vm = vms.first(where: { $0.name == name }) {
                    detail(vm)
                } else {
                    EmptyHint(icon: "macwindow", text: vms.isEmpty ? "No macOS VMs yet" : "Select a macOS VM")
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .navigationTitle("macOS VMs")
        .toolbar {
            Button { sheet = MacSheet(job: nil) } label: { Label("New macOS VM", systemImage: "plus") }
        }
        .sheet(item: $sheet) { s in CreateMacVMSheet(job: s.job) }
        .sheet(item: $report) { r in ReportSheet(report: r) { report = nil } }
        .confirmationDialog("Delete \(confirmDelete?.name ?? "")?",
                            isPresented: Binding(get: { confirmDelete != nil }, set: { if !$0 { confirmDelete = nil } }),
                            presenting: confirmDelete) { vm in
            Button("Delete macOS VM", role: .destructive) { act("Delete", vm.name, ["delete", vm.name, "--yes"]) }
        } message: { vm in
            Text(vms.contains { $0.base == vm.name }
                ? "Its disk and everything in it are deleted. Clones made from it keep working. This can't be undone."
                : "Its disk and everything in it are deleted. This can't be undone.")
        }
        .confirmationDialog("Reset \(confirmReset?.name ?? "")?",
                            isPresented: Binding(get: { confirmReset != nil }, set: { if !$0 { confirmReset = nil } }),
                            presenting: confirmReset) { vm in
            Button("Reset", role: .destructive) { act("Reset", vm.name, ["reset", vm.name]) }
        } message: { vm in
            Text("It goes back to \(vm.base.map { "a fresh copy of \($0)" } ?? "its base image (how it was when created, or when last made a base image)"). Everything changed in it since is lost.")
        }
        .alert("Clone \(cloning?.name ?? "")", isPresented: Binding(get: { cloning != nil }, set: { if !$0 { cloning = nil } })) {
            TextField("Name of the clone", text: $cloneName)
            Button("Clone") {
                if let vm = cloning, MacVMs.validName(cloneName) {
                    let name = cloneName
                    act("Clone", vm.name, ["clone", vm.name, name]) { selection = name }
                }
                cloning = nil
            }
            Button("Cancel", role: .cancel) { cloning = nil }
        } message: {
            Text("A copy-on-write copy: instant, and it takes space only as it changes. 1–32 characters: a–z, 0–9 and -.")
        }
        .task(id: creates.finished) {
            while !Task.isCancelled {
                await refresh()
                try? await Task.sleep(for: .seconds(4))
            }
        }
        .onAppear {
            if UserDefaults.standard.string(forKey: "openSheet") == "NewMacVM" { sheet = MacSheet(job: nil) }
        }
    }

    @ViewBuilder
    private func menu(_ vm: MacVM) -> some View {
        if vm.isRunning {
            Button("Show Display") { act("Show display", vm.name, ["start", vm.name, "--display"]) }
            Button("Stop") { act("Stop", vm.name, ["stop", vm.name]) }
        } else {
            Button("Start") { act("Start", vm.name, ["start", vm.name]) }
            Button("Start with Display") { act("Start", vm.name, ["start", vm.name, "--display"]) }
        }
        Divider()
        Button("Info…") { showInfo(vm.name) }
        Button("Clone…") { cloneName = "\(vm.name)-2"; cloning = vm }
        Button("Make Base Image") { act("Make base image", vm.name, ["set-base", vm.name]) }
        Button("Reset…") { confirmReset = vm }
        if let ip = vm.ip { Button("Copy IP (\(ip))") { copy(ip) } }
        Divider()
        Button("Delete…", role: .destructive) { confirmDelete = vm }
    }

    private func detail(_ vm: MacVM) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 12) {
                Image(systemName: "macwindow").font(.system(size: 34)).foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    Text(vm.name).font(.title2.weight(.semibold))
                    HStack(spacing: 6) {
                        StatusDot(running: vm.isRunning, size: 7)
                        Text(vm.isRunning ? "Running" : "Stopped").foregroundStyle(.secondary)
                    }
                }
                Spacer()
                if busy.contains(vm.name) { ProgressView().controlSize(.small) }
            }
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 6) {
                fact("CPUs", "\(vm.cpus)")
                fact("Memory", MacVMs.gib(mib: vm.memoryMib))
                fact("Disk", "\(vm.diskGib) GiB")
                fact("Cloned from", vm.base ?? "—")
                fact("IP address", vm.ip ?? (vm.isRunning ? "waiting…" : "—"))
            }
            HStack {
                if vm.isRunning {
                    Button { act("Show display", vm.name, ["start", vm.name, "--display"]) } label: {
                        Label("Show Display", systemImage: "rectangle.on.rectangle")
                    }
                    Button { act("Stop", vm.name, ["stop", vm.name]) } label: { Label("Stop", systemImage: "stop.fill") }
                } else {
                    Button { act("Start", vm.name, ["start", vm.name, "--display"]) } label: {
                        Label("Start with Display", systemImage: "play.rectangle")
                    }
                    .help("Starts it in a window you can use")
                    .disabled(runningCount >= MacVMs.runningLimit)
                    Button { act("Start", vm.name, ["start", vm.name]) } label: { Label("Start", systemImage: "play.fill") }
                        .help("Starts it without a window, for agents")
                        .disabled(runningCount >= MacVMs.runningLimit)
                }
            }
            .disabled(busy.contains(vm.name))
            HStack {
                Button("Info…") { showInfo(vm.name) }
                Button("Clone…") { cloneName = "\(vm.name)-2"; cloning = vm }
                Button("Make Base Image") { act("Make base image", vm.name, ["set-base", vm.name]) }
                    .help("Reset and new clones start from how it is now")
                Button("Reset…") { confirmReset = vm }
                    .help("Back to its base image in about a second")
                Spacer()
                Button("Delete…", role: .destructive) { confirmDelete = vm }
            }
            .disabled(busy.contains(vm.name))
            if !vm.isRunning && runningCount >= MacVMs.runningLimit {
                Text("Two macOS VMs are running, the most Apple's license allows. Stop one to start another.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(20)
    }

    private func fact(_ label: String, _ value: String) -> some View {
        GridRow {
            Text(label).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }

    private func refresh() async {
        do {
            let list = try await MacVMs.list().sorted { $0.name < $1.name }
            if list != vms { vms = list }
            loadError = nil
        } catch {
            loadError = error.localizedDescription
        }
        loaded = true
    }

    private func act(_ what: String, _ name: String, _ args: [String], then: (() -> Void)? = nil) {
        busy.insert(name)
        Task {
            if let error = await MacVMs.run(args) {
                engine.lastError = "\(what) \(name): \(error)"
            } else {
                if args.first == "delete", selection == name { selection = nil }
                then?()
            }
            busy.remove(name)
            await refresh()
        }
    }

    /// The CLI's own rendering, so the app shows what `berth mac info` prints.
    private func showInfo(_ name: String) {
        Task {
            let result = await offPool { Engine.run([BerthPaths.cli, "mac", "info", name]) }
            report = MachineReport(title: name, text: result.output.isEmpty ? "(nothing)" : result.output)
        }
    }
}

struct MacSheet: Identifiable {
    let id = UUID()
    /// A create under way to show; nil for a new one.
    let job: MacCreateJob?
}

private struct CreateJobRow: View {
    @ObservedObject var job: MacCreateJob
    let show: () -> Void

    var body: some View {
        HStack {
            if job.running {
                ProgressView().controlSize(.small)
            } else {
                Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.yellow)
            }
            Text(job.running ? "Creating \(job.name)…" : "Creating \(job.name) failed").foregroundStyle(.secondary)
            Spacer()
            Button(job.running ? "Show" : "Details") { show() }.buttonStyle(.borderless)
        }
        .contextMenu {
            if !job.running { Button("Dismiss") { MacCreateJobs.shared.dismiss(job) } }
        }
    }
}

/// A CLI report (info) in a sheet; the same look as the machines' one.
struct ReportSheet: View {
    let report: MachineReport
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(report.title).font(.headline)
            ScrollView {
                Text(report.text).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            HStack { Spacer(); Button("Close") { close() }.keyboardShortcut(.defaultAction) }
        }
        .padding()
        .frame(width: 620, height: 420)
    }
}

/// The form, then the CLI's progress as it comes.
struct CreateMacVMSheet: View {
    @Environment(\.dismiss) private var dismiss
    @State private var job: MacCreateJob?
    @State private var name = ""
    @State private var cpus = min(4, ProcessInfo.processInfo.activeProcessorCount)
    @State private var memoryGiB = 8
    @State private var diskGiB = 64
    @State private var ipsw: URL?
    @State private var pickingIPSW = false

    init(job: MacCreateJob?) { _job = State(initialValue: job) }

    private let hostCPUs = ProcessInfo.processInfo.activeProcessorCount
    private let hostMemoryGiB = Int(ProcessInfo.processInfo.physicalMemory >> 30)

    var body: some View {
        if let job {
            CreateProgress(job: job) { dismiss() }
        } else {
            form
        }
    }

    private var form: some View {
        Form {
            TextField("Name", text: $name, prompt: Text("e.g. test"))
            Stepper("CPUs: \(cpus)", value: $cpus, in: 2...max(2, hostCPUs))
            Picker("Memory", selection: $memoryGiB) {
                ForEach([4, 6, 8, 12, 16, 24, 32].filter { $0 <= max(4, hostMemoryGiB / 2) }, id: \.self) { Text("\($0) GiB").tag($0) }
            }
            Picker("Disk", selection: $diskGiB) {
                ForEach([40, 64, 100, 160, 256], id: \.self) { Text("\($0) GiB").tag($0) }
            }
            LabeledContent("macOS") {
                HStack {
                    Text(ipsw?.lastPathComponent ?? "Latest for this Mac").foregroundStyle(.secondary).lineLimit(1)
                    if ipsw != nil { Button("Clear") { ipsw = nil } }
                    Button("Choose IPSW…") { pickingIPSW = true }
                }
                .fileImporter(isPresented: $pickingIPSW, allowedContentTypes: [.item]) { result in
                    if case let .success(url) = result { ipsw = url }
                }
            }
            Text(ipsw == nil
                ? "Downloads the latest macOS this Mac can run (about 15 GB), installs it and sets up a user who logs in automatically. The disk takes space only as it fills."
                : "Installs from the chosen restore image and sets up a user who logs in automatically. The disk takes space only as it fills.")
                .font(.caption).foregroundStyle(.secondary)
            if !name.isEmpty && !MacVMs.validName(name) {
                Text("1–32 characters: a–z, 0–9 and -.").font(.caption).foregroundStyle(.red)
            }
        }
        .formStyle(.grouped)
        .frame(width: 440)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Create") {
                    job = MacCreateJobs.shared.start(NewMacVM(name: name, cpus: cpus, memoryMib: memoryGiB * 1024,
                                                              diskGib: diskGiB, ipsw: ipsw))
                }
                .keyboardShortcut(.defaultAction).disabled(!MacVMs.validName(name))
            }
            .padding()
        }
    }
}

private struct CreateProgress: View {
    @ObservedObject var job: MacCreateJob
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(job.running ? "Creating \(job.name)…" : job.failed ? "Creating \(job.name) failed" : "\(job.name) is ready")
                .font(.headline)
            if job.running {
                if let fraction = job.console.fraction {
                    ProgressView(value: fraction)
                } else {
                    ProgressView().progressViewStyle(.linear)
                }
            }
            ScrollViewReader { proxy in
                ScrollView {
                    Text(job.console.text.isEmpty ? "Starting…" : job.console.text)
                        .font(.system(.caption, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .textSelection(.enabled)
                    Color.clear.frame(height: 1).id("end")
                }
                .onChange(of: job.console) { proxy.scrollTo("end", anchor: .bottom) }
            }
            .frame(height: 240)
            .padding(6)
            .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
            HStack {
                if job.running {
                    Text("It keeps going if you close this.").font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                if job.running { Button("Stop") { job.stop() } }
                if job.failed { Button("Dismiss") { MacCreateJobs.shared.dismiss(job); close() } }
                Button("Close") { close() }.keyboardShortcut(.defaultAction)
            }
        }
        .padding()
        .frame(width: 560)
    }
}
