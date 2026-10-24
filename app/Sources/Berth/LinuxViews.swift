import AppKit
import SwiftUI

// MARK: - Machines

struct LinuxMachine: Decodable, Identifiable, Hashable {
    let name: String
    let state: String
    let os: String
    let addresses: [String]
    var id: String { name }
    var isRunning: Bool { state == "running" }
}

/// `berth machine …` through the daemon's control socket.
enum Machines {
    struct Reply: Decodable { let output: String }

    static func call(_ action: String, _ args: [String] = []) async throws -> String {
        let reply: Reply = try await Task.detached {
            let sock = try UnixSocket(path: BerthPaths.controlSocket)
            var tv = timeval(tv_sec: 900, tv_usec: 0)
            setsockopt(sock.fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
            let request = try JSONSerialization.data(withJSONObject: ["cmd": "machine", "action": action, "args": args])
            try sock.write(request + Data("\n".utf8))
            var data = Data()
            while !data.contains(0x0a), let chunk = try sock.read() { data.append(chunk) }
            return try JSONDecoder().decode(Reply.self, from: data)
        }.value
        if reply.output.hasPrefix("error: ") { throw SocketError.message(String(reply.output.dropFirst(7))) }
        return reply.output
    }

    static func list() async throws -> [LinuxMachine] {
        try JSONDecoder().decode([LinuxMachine].self, from: Data(try await call("list").utf8))
    }
}

struct MachinesView: View {
    @EnvironmentObject var engine: Engine
    @State private var machines: [LinuxMachine] = []
    // `-openMachine dev` picks it; `-openSheet NewMachine|MachineInfo|MachineLogs`.
    @State private var selection: String? = UserDefaults.standard.string(forKey: "openMachine") ?? "berth-vm"
    @State private var showCreate = false
    /// Machines with an action under way, and ones being created (minutes).
    @State private var busy: Set<String> = []
    @State private var creating: [String] = []
    @State private var confirmDelete: String?
    @State private var renaming: String?
    @State private var newName = ""
    /// `berth machine info|logs` output, shown in a sheet.
    @State private var report: MachineReport?

    var body: some View {
        HStack(spacing: 0) {
            List(selection: $selection) {
                SwiftUI.Section("Virtual machine") {
                    Label("berth (Debian 13)", systemImage: "server.rack").tag("berth-vm")
                }
                SwiftUI.Section("Machines") {
                    ForEach(machines) { m in
                        HStack {
                            Circle().fill(m.isRunning ? .green : .gray).frame(width: 7, height: 7)
                            VStack(alignment: .leading, spacing: 1) {
                                Text(m.name).fontWeight(.medium)
                                Text(m.os).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                            }
                            Spacer()
                            if busy.contains(m.name) { ProgressView().controlSize(.small) }
                        }
                        .tag(m.name)
                        .contextMenu {
                            if m.isRunning {
                                Button("Stop") { act("stop", m.name) }
                                Button("Restart") { act("restart", m.name) }
                                Button("Open \(m.name).berth.local") {
                                    NSWorkspace.shared.open(URL(string: "http://\(m.name).berth.local")!)
                                }
                            } else {
                                Button("Start") { act("start", m.name) }
                            }
                            Button("Open in Finder") {
                                NSWorkspace.shared.open(URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Berth/machines/\(m.name)"))
                            }
                            Button("Copy Domain") { copy("\(m.name).berth.local") }
                            Divider()
                            Button("Info…") { showReport(m.name, ["info", m.name], title: "\(m.name)") }
                            Button("Logs…") { showReport(m.name, ["logs", m.name, "-n", "300"], title: "\(m.name) logs") }
                            Button("Rename…") { newName = m.name; renaming = m.name }
                            Divider()
                            Button("Delete…", role: .destructive) { confirmDelete = m.name }
                        }
                    }
                    ForEach(creating, id: \.self) { name in
                        HStack {
                            ProgressView().controlSize(.small)
                            Text("Creating \(name)…").foregroundStyle(.secondary)
                        }
                    }
                    if machines.isEmpty && creating.isEmpty {
                        Text("No machines yet").foregroundStyle(.secondary)
                    }
                }
            }
            .frame(width: 260)
            .toolbar {
                Button { showCreate = true } label: { Label("New Machine", systemImage: "plus") }
            }
            Divider()
            Group {
                if selection == "berth-vm" {
                    MachineTerminal(args: ["shell"]).id("vm").terminalInset()
                } else if let name = selection, let m = machines.first(where: { $0.name == name }) {
                    if m.isRunning {
                        VStack(spacing: 0) {
                            HStack {
                                Text(m.name).font(.headline)
                                Text(m.addresses.first.map { "\(m.name).berth.local · \($0)" } ?? "")
                                    .font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                                Spacer()
                            }
                            .padding(10)
                            Divider()
                            MachineTerminal(args: ["machine", "shell", m.name]).id(m.name).terminalInset()
                        }
                    } else {
                        VStack(spacing: 12) {
                            Text("\(m.name) is stopped").font(.title3)
                            Button("Start") { act("start", m.name) }
                        }
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                } else {
                    EmptyHint(icon: "terminal", text: "Select a machine")
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .sheet(isPresented: $showCreate) { CreateMachineSheet { create($0) } }
        .sheet(item: $report) { r in
            VStack(alignment: .leading, spacing: 10) {
                Text(r.title).font(.headline)
                ScrollView {
                    Text(r.text).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                HStack { Spacer(); Button("Close") { report = nil }.keyboardShortcut(.defaultAction) }
            }
            .padding()
            .frame(width: 620, height: 420)
        }
        .confirmationDialog("Delete \(confirmDelete ?? "")?", isPresented: Binding(get: { confirmDelete != nil }, set: { if !$0 { confirmDelete = nil } })) {
            Button("Delete Machine", role: .destructive) {
                if let name = confirmDelete { act("delete", name) }
                confirmDelete = nil
            }
        } message: {
            Text("Its files and everything installed in it are deleted. This can't be undone.")
        }
        .alert("Rename \(renaming ?? "")", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
            TextField("New name", text: $newName)
            Button("Rename") {
                if let old = renaming { act("rename", old, [old, newName]) }
                renaming = nil
            }
            Button("Cancel", role: .cancel) { renaming = nil }
        } message: {
            Text("1–12 characters: a–z, 0–9 and -. A running machine restarts.")
        }
        .task {
            while !Task.isCancelled {
                await refresh()
                try? await Task.sleep(for: .seconds(4))
            }
        }
        .navigationTitle("Machines")
        .onAppear {
            switch UserDefaults.standard.string(forKey: "openSheet") {
            case "NewMachine": showCreate = true
            case "MachineInfo": if let m = selection, m != "berth-vm" { showReport(m, ["info", m], title: m) }
            case "MachineLogs": if let m = selection, m != "berth-vm" { showReport(m, ["logs", m, "-n", "300"], title: "\(m) logs") }
            default: break
            }
        }
    }

    private func refresh() async {
        if let list = try? await Machines.list() { machines = list }
    }

    private func act(_ action: String, _ name: String, _ args: [String]? = nil) {
        busy.insert(name)
        Task {
            do { _ = try await Machines.call(action, args ?? [name]) } catch { engine.lastError = "\(action) \(name): \(error.localizedDescription)" }
            if action == "delete" || action == "rename", selection == name { selection = action == "rename" ? args?.last : "berth-vm" }
            busy.remove(name)
            try? await Task.sleep(for: .seconds(2))
            await refresh()
        }
    }

    /// The CLI's own rendering, so the app shows what `berth machine` prints.
    private func showReport(_ name: String, _ args: [String], title: String) {
        Task {
            let result = await Task.detached { Engine.run([BerthPaths.cli, "machine"] + args) }.value
            report = MachineReport(title: title, text: result.output.isEmpty ? "(nothing)" : result.output)
        }
    }

    private func create(_ new: NewMachine) {
        let name = new.name
        creating.append(name)
        Task {
            let user = NSUserName()
            do {
                // Same arguments as `berth machine create` (src/machines.rs).
                _ = try await Machines.call("create", [
                    name, new.distro, user, String(getuid()), NSHomeDirectory(), new.arch, new.userData,
                    new.isolated || new.isolateNetwork ? "1" : "", new.isolateNetwork ? "1" : "",
                    new.mounts.map(\.path).joined(separator: "\n"),
                ])
                try? await Task.sleep(for: .seconds(3))
                await refresh()
                selection = name
            } catch {
                engine.lastError = "Create \(name): \(error.localizedDescription)"
            }
            creating.removeAll { $0 == name }
        }
    }
}

struct MachineReport: Identifiable {
    let id = UUID()
    let title: String
    let text: String
}

struct NewMachine {
    var name: String
    var distro: String
    var arch: String
    var userData: String
    var isolated: Bool
    var isolateNetwork: Bool
    var mounts: [URL]
}

struct CreateMachineSheet: View {
    @Environment(\.dismiss) private var dismiss
    let create: (NewMachine) -> Void
    @State private var isolated = false
    @State private var isolateNetwork = false
    @State private var mounts: [URL] = []
    @State private var name = ""
    @State private var distro = "ubuntu:noble"
    @State private var arch = "arm64"
    @State private var cloudInit: URL?
    @State private var cloudInitError: String?
    /// Read when chosen: a file that later becomes unreadable mustn't turn
    /// into "no cloud-init" silently.
    @State private var userData = ""
    @State private var pickingMounts = false
    @State private var pickingCloudInit = false

    /// Network isolation implies isolation.
    private var isIsolated: Bool { isolated || isolateNetwork }

    private let distros = [
        ("Ubuntu 24.04", "ubuntu:noble"), ("Debian 13", "debian:trixie"), ("Fedora 44", "fedora:44"),
        ("Alpine", "alpine"), ("Arch Linux", "archlinux"), ("Rocky Linux 10", "rockylinux:10"),
        ("AlmaLinux 10", "almalinux:10"), ("openSUSE Tumbleweed", "opensuse:tumbleweed"),
    ]

    /// The path the VM sees: realpath keeps /private (/tmp is the VM's own
    /// there), where URL.resolvingSymlinksInPath drops it.
    private func vmPath(_ url: URL) -> URL {
        guard let real = realpath(url.path, nil) else { return url }
        defer { free(real) }
        return URL(fileURLWithPath: String(cString: real))
    }

    private func cloudInitChosen(_ url: URL) {
        cloudInit = url
        let text = (try? String(contentsOf: url, encoding: .utf8)) ?? ""
        userData = text
        cloudInitError = text.hasPrefix("#cloud-config") || text.hasPrefix("#!") ? nil : "Starts with #cloud-config (or #! for a script)."
    }

    private var validName: Bool {
        (1...12).contains(name.count) && name.allSatisfy { $0.isASCII && ($0.isLowercase || $0.isNumber || $0 == "-") } && !name.hasPrefix("-")
    }

    var body: some View {
        Form {
            TextField("Name", text: $name, prompt: Text("e.g. dev"))
            Picker("Distribution", selection: $distro) {
                // Isolated machines need systemd (berth reaches into them only through it).
                ForEach(distros.filter { !isIsolated || $0.1 != "alpine" }, id: \.1) { Text($0.0).tag($0.1) }
            }
            .onChange(of: isIsolated) { _, on in if on && distro == "alpine" { distro = "debian:trixie" } }
            Picker("Architecture", selection: $arch) {
                Text("Apple silicon (arm64)").tag("arm64")
                Text("Intel (amd64, with Rosetta)").tag("amd64")
            }
            LabeledContent("cloud-init") {
                HStack {
                    Text(cloudInit?.lastPathComponent ?? "None").foregroundStyle(.secondary)
                    if cloudInit != nil { Button("Clear") { cloudInit = nil; cloudInitError = nil; userData = "" } }
                    Button("Choose…") { pickingCloudInit = true }
                }
                // One importer per view: SwiftUI keeps only the last of several.
                .fileImporter(isPresented: $pickingCloudInit, allowedContentTypes: [.item]) { result in
                    if case let .success(url) = result { cloudInitChosen(url) }
                }
            }
            if let cloudInitError { Text(cloudInitError).font(.caption).foregroundStyle(.red) }
            Toggle("Isolated from the Mac", isOn: Binding(get: { isIsolated }, set: { isolated = $0; if !$0 { isolateNetwork = false } }))
                .help("No home folder, mac command or SSH agent; only the folders below. Needs a systemd distribution (not Alpine).")
            if isIsolated {
                LabeledContent("Shared folders") {
                    HStack {
                        Text(mounts.isEmpty ? "None" : mounts.map(\.lastPathComponent).joined(separator: ", ")).foregroundStyle(.secondary)
                        if !mounts.isEmpty { Button("Clear") { mounts = [] } }
                        Button("Add…") { pickingMounts = true }
                    }
                }
            }
            Toggle("Isolate its network too", isOn: $isolateNetwork)
                .help("DNS and the internet only: not the Mac, the LAN or other private networks.")
            Text(isIsolated
                ? "Your user and sudo are set up inside; only the folders above are shared. Reachable at \(name.isEmpty ? "<name>" : name).berth.local; `ssh \(name.isEmpty ? "<name>" : name).berth` too."
                : "Your user, sudo, and your Mac home folder are set up inside. Reachable at \(name.isEmpty ? "<name>" : name).berth.local; `ssh \(name.isEmpty ? "<name>" : name).berth` too.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
        .fileImporter(isPresented: $pickingMounts, allowedContentTypes: [.folder], allowsMultipleSelection: true) { result in
            if case let .success(urls) = result { mounts += urls.map(vmPath) }
        }
        .frame(width: 420)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Create") {
                    create(NewMachine(name: name, distro: distro, arch: arch, userData: cloudInit == nil ? "" : userData,
                                      isolated: isIsolated, isolateNetwork: isolateNetwork, mounts: isIsolated ? mounts : []))
                    dismiss()
                }
                    .keyboardShortcut(.defaultAction).disabled(!validName || cloudInitError != nil)
            }
            .padding()
        }
    }
}

// MARK: - Kubernetes

struct Pod: Identifiable, Hashable {
    let namespace: String
    let name: String
    let phase: String
    let ip: String
    var id: String { "\(namespace)/\(name)" }
}

struct KubernetesView: View {
    @EnvironmentObject var engine: Engine
    @State private var status = "…"
    @State private var pods: [Pod] = []
    @State private var working = false

    private var kubectl: String { BerthPaths.root.appendingPathComponent("bin/kubectl").path }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 12) {
                Image(systemName: "helm").font(.title2)
                VStack(alignment: .leading) {
                    Text("Kubernetes").font(.headline)
                    Text(status == "running" ? "Running · context berth · localhost:6443" : status.capitalized)
                        .font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                if working { ProgressView().controlSize(.small) }
                Button(status == "running" ? "Disable" : "Enable") { toggle() }.disabled(working)
            }
            .padding(14)
            Divider()
            if status == "running" {
                Table(pods) {
                    TableColumn("Namespace", value: \.namespace).width(140)
                    TableColumn("Pod", value: \.name)
                    TableColumn("Status", value: \.phase).width(90)
                    TableColumn("IP") { Text($0.ip).monospaced() }.width(120)
                }
            } else {
                EmptyHint(icon: "helm", text: "A single-node cluster (k3s) using berth's Docker:\nimages you build locally run in pods without a registry.")
            }
        }
        .task {
            while !Task.isCancelled {
                await refresh()
                try? await Task.sleep(for: .seconds(3))
            }
        }
        .navigationTitle("Kubernetes")
    }

    private func refresh() async {
        let s = await Task.detached { Engine.run([BerthPaths.cli, "k8s", "status"]).output }.value
        status = s.replacingOccurrences(of: "Kubernetes: ", with: "").trimmingCharacters(in: .whitespacesAndNewlines)
        guard status == "running" else { pods = []; return }
        let json = await Task.detached { [kubectl] in
            Engine.run([kubectl, "--context", "berth", "get", "pods", "-A", "-o", "json"]).output
        }.value
        let items = ((try? JSONSerialization.jsonObject(with: Data(json.utf8))) as? [String: Any])?["items"] as? [[String: Any]] ?? []
        pods = items.map { item in
            let meta = item["metadata"] as? [String: Any] ?? [:]
            let st = item["status"] as? [String: Any] ?? [:]
            return Pod(namespace: meta["namespace"] as? String ?? "", name: meta["name"] as? String ?? "",
                       phase: st["phase"] as? String ?? "", ip: st["podIP"] as? String ?? "")
        }
    }

    private func toggle() {
        working = true
        let action = status == "running" ? "disable" : "enable"
        Task {
            let result = await Task.detached { Engine.run([BerthPaths.cli, "k8s", action]) }.value
            if result.code != 0 { engine.lastError = "Kubernetes: \(result.output.trimmingCharacters(in: .whitespacesAndNewlines))" }
            working = false
            await refresh()
        }
    }
}
