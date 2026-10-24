import Charts
import SwiftUI

struct ContainerDetail: View {
    @EnvironmentObject var engine: Engine
    let container: Container
    @State private var tab = Tab(rawValue: UserDefaults.standard.string(forKey: "openTab") ?? "") ?? .info
    /// `berth debug`: the VM's tools in the container, for images without a shell.
    @State private var debugShell = false

    enum Tab: String, CaseIterable {
        case info = "Info", logs = "Logs", terminal = "Terminal", files = "Files", stats = "Stats"
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .firstTextBaseline) {
                Circle().fill(container.isRunning ? .green : .gray).frame(width: 10, height: 10)
                Text(container.name).font(.title2.weight(.semibold)).textSelection(.enabled)
                Text(container.status).foregroundStyle(.secondary)
                Spacer()
                ContainerActions(container: container)
                Menu { ContainerMenu(container: container) } label: { Image(systemName: "ellipsis.circle") }
                    .menuStyle(.borderlessButton).frame(width: 30)
            }
            .padding([.horizontal, .top], 16)

            Picker("", selection: $tab) {
                ForEach(Tab.allCases, id: \.self) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(16)

            Divider()

            Group {
                switch tab {
                case .info: ContainerInfo(container: container)
                case .logs: LogsView(containerID: container.id, running: container.isRunning)
                case .terminal:
                    if container.isRunning {
                        VStack(alignment: .leading, spacing: 8) {
                            Picker("", selection: $debugShell) {
                                Text("Shell").tag(false)
                                Text("Debug shell").tag(true)
                            }
                            .pickerStyle(.segmented)
                            .labelsHidden()
                            .fixedSize()
                            .padding(.horizontal, 16)
                            .help("Debug shell: a root shell with the VM's tools (bash, ps, curl, apt) in the container's namespaces, even for images without a shell. Its files are at /container.")
                            if debugShell {
                                MachineTerminal(args: ["debug", container.id]).id("debug-\(container.id)")
                            } else {
                                ContainerTerminal(containerID: container.id)
                            }
                        }
                    } else {
                        EmptyHint(icon: "terminal", text: "Start the container to open a terminal.")
                    }
                case .files:
                    if container.isRunning {
                        FilesView(containerID: container.id)
                    } else {
                        EmptyHint(icon: "folder", text: "Start the container to browse its files.")
                    }
                case .stats:
                    if container.isRunning {
                        StatsView(containerID: container.id)
                    } else {
                        EmptyHint(icon: "chart.xyaxis.line", text: "Start the container to see live usage.")
                    }
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

// MARK: Info

struct ContainerInfo: View {
    @EnvironmentObject var engine: Engine
    let container: Container

    var body: some View {
        Form {
            SwiftUI.Section("General") {
                LabeledContent("Image", value: container.image)
                LabeledContent("ID") { Text(String(container.id.prefix(12))).monospaced().textSelection(.enabled) }
                LabeledContent("Created", value: Date(timeIntervalSince1970: TimeInterval(container.created)).formatted())
                if let project = container.composeProject {
                    LabeledContent("Compose", value: "\(project) / \(container.composeService ?? "")")
                }
            }
            SwiftUI.Section("Network") {
                let domains = engine.domains(for: container)
                if domains.isEmpty {
                    LabeledContent("Domain", value: container.isRunning ? "—" : "available while running")
                }
                ForEach(domains) { d in
                    LabeledContent("Domain") {
                        Link(d.domain, destination: URL(string: "\(TLSTrust.scheme)://\(d.domain)")!)
                    }
                }
                ForEach(container.ipAddresses, id: \.self) { ip in
                    LabeledContent("IP") { Text(ip).monospaced().textSelection(.enabled) }
                }
                ForEach(container.publishedPorts, id: \.publicPort) { p in
                    // Verbatim strings: interpolated numbers in a localized key get
                    // thousands separators ("8.080").
                    LabeledContent(String("Port \(p.privatePort)/\(p.type)")) {
                        Link(String("localhost:\(p.publicPort!)"), destination: URL(string: "http://localhost:\(p.publicPort!)")!)
                    }
                }
            }
            if let labels = container.labels, !labels.isEmpty {
                SwiftUI.Section("Labels") {
                    ForEach(labels.keys.sorted(), id: \.self) { key in
                        LabeledContent(key) { Text(labels[key]!).textSelection(.enabled).lineLimit(2) }
                    }
                }
            }
        }
        .formStyle(.grouped)
    }
}

// MARK: Logs

struct LogsView: View {
    let containerID: String
    var running = true
    @State private var text = ""
    @State private var follow = true
    @State private var error: String?
    private let limit = 400_000

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    Text(text.isEmpty ? (error ?? "No output yet.") : text)
                        .font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(10)
                    Color.clear.frame(height: 1).id("end")
                }
                .onChange(of: text) {
                    if follow { proxy.scrollTo("end", anchor: .bottom) }
                }
            }
            Divider()
            HStack {
                Toggle("Follow", isOn: $follow).toggleStyle(.checkbox)
                Spacer()
                Button("Copy") { copy(text) }
                Button("Clear") { text = "" }
            }
            .padding(8)
        }
        // Again when the container starts again (its log stream ended).
        .task(id: "\(containerID)-\(running)") {
            text = ""
            // Chunks are gathered and shown every 100 ms: a chatty container
            // would otherwise re-render the whole text per line.
            let buffer = LogBuffer()
            await withTaskGroup(of: Void.self) { group in
                group.addTask { @MainActor in
                    do {
                        for try await chunk in Docker.logs(containerID) { buffer.pending += chunk }
                    } catch {
                        self.error = error.localizedDescription
                    }
                    buffer.done = true
                }
                group.addTask { @MainActor in
                    while !Task.isCancelled {
                        flush(buffer)
                        if buffer.done { break }
                        try? await Task.sleep(for: .milliseconds(100))
                    }
                }
            }
        }
    }

    /// Append, keeping the last `limit` bytes (utf8.count is constant-time;
    /// count, in characters, walks the whole string).
    private func flush(_ buffer: LogBuffer) {
        guard !buffer.pending.isEmpty else { return }
        text += buffer.pending
        buffer.pending = ""
        if text.utf8.count > limit { text = String(text.suffix(limit / 2)) }
    }
}

/// Log text not shown yet (LogsView).
@MainActor
final class LogBuffer {
    var pending = ""
    var done = false
}

// MARK: Stats

struct StatsView: View {
    let containerID: String
    @State private var samples: [StatsSample] = []
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            if let error { Text(error).foregroundStyle(.red) }
            let last = samples.last
            HStack(spacing: 30) {
                Metric(title: "CPU", value: last.map { String(format: "%.1f%%", $0.cpuPercent) } ?? "—")
                Metric(title: "Memory", value: last.map { formatBytes($0.memoryBytes) } ?? "—")
                if let limit = last?.memoryLimit, limit > 0 {
                    Metric(title: "Limit", value: formatBytes(limit))
                }
            }
            Text("CPU %").font(.headline)
            Chart(samples) { s in
                AreaMark(x: .value("Time", s.time), y: .value("CPU", s.cpuPercent))
                    .foregroundStyle(.blue.opacity(0.25))
                LineMark(x: .value("Time", s.time), y: .value("CPU", s.cpuPercent))
            }
            .chartYScale(domain: 0...max(100, samples.map(\.cpuPercent).max() ?? 0))
            .frame(minHeight: 120)
            Text("Memory").font(.headline)
            Chart(samples) { s in
                AreaMark(x: .value("Time", s.time), y: .value("Memory", s.memoryBytes / 1_048_576))
                    .foregroundStyle(.green.opacity(0.25))
                LineMark(x: .value("Time", s.time), y: .value("Memory", s.memoryBytes / 1_048_576))
                    .foregroundStyle(.green)
            }
            .chartYAxisLabel("MiB")
            .frame(minHeight: 120)
        }
        .padding(16)
        .task(id: containerID) {
            while !Task.isCancelled {
                do {
                    let sample = try await Docker.stats(containerID)
                    samples.append(sample)
                    if samples.count > 90 { samples.removeFirst(samples.count - 90) }
                    error = nil
                } catch {
                    self.error = error.localizedDescription
                }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }
}

struct Metric: View {
    let title: String
    let value: String

    var body: some View {
        VStack(alignment: .leading) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            Text(value).font(.title2.monospacedDigit())
        }
    }
}

func formatBytes<T: BinaryInteger>(_ bytes: T) -> String {
    ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .binary)
}

func formatBytes(_ bytes: Double) -> String { formatBytes(Int64(bytes)) }
