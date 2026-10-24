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
    @State private var selection: String? = "berth-vm"
    @State private var showCreate = false
    @State private var busy: String?

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
                            if busy == m.name { ProgressView().controlSize(.small) }
                        }
                        .tag(m.name)
                        .contextMenu {
                            if m.isRunning {
                                Button("Stop") { act("stop", m.name) }
                                Button("Open \(m.name).berth.local") {
                                    NSWorkspace.shared.open(URL(string: "http://\(m.name).berth.local")!)
                                }
                            } else {
                                Button("Start") { act("start", m.name) }
                            }
                            Button("Copy Domain") { copy("\(m.name).berth.local") }
                            Divider()
                            Button("Delete", role: .destructive) { act("delete", m.name) }
                        }
                    }
                    if machines.isEmpty {
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
                    MachineTerminal(args: ["shell"]).id("vm")
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
                            MachineTerminal(args: ["machine", "shell", m.name]).id(m.name)
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
        .sheet(isPresented: $showCreate) { CreateMachineSheet { name, distro, arch, userData in create(name, distro, arch, userData) } }
        .task {
            while !Task.isCancelled {
                await refresh()
                try? await Task.sleep(for: .seconds(4))
            }
        }
        .navigationTitle("Machines")
    }

    private func refresh() async {
        if let list = try? await Machines.list() { machines = list }
    }

    private func act(_ action: String, _ name: String) {
        busy = name
        Task {
            do { _ = try await Machines.call(action, [name]) } catch { engine.lastError = "\(action) \(name): \(error.localizedDescription)" }
            if action == "delete", selection == name { selection = "berth-vm" }
            busy = nil
            try? await Task.sleep(for: .seconds(2))
            await refresh()
        }
    }

    private func create(_ name: String, _ distro: String, _ arch: String, _ userData: String) {
        busy = name
        Task {
            let user = NSUserName()
            do {
                // Same arguments as `berth machine create` (src/machines.rs).
                _ = try await Machines.call("create", [name, distro, user, String(getuid()), NSHomeDirectory(), arch, userData])
                try? await Task.sleep(for: .seconds(3))
                await refresh()
                selection = name
            } catch {
                engine.lastError = "Create \(name): \(error.localizedDescription)"
            }
            busy = nil
        }
    }
}

struct CreateMachineSheet: View {
    @Environment(\.dismiss) private var dismiss
    let create: (String, String, String, String) -> Void
    @State private var name = ""
    @State private var distro = "ubuntu:noble"
    @State private var arch = "arm64"
    @State private var cloudInit: URL?
    @State private var cloudInitError: String?

    private let distros = [
        ("Ubuntu 24.04", "ubuntu:noble"), ("Debian 13", "debian:trixie"), ("Fedora 44", "fedora:44"),
        ("Alpine", "alpine"), ("Arch Linux", "archlinux"), ("Rocky Linux 10", "rockylinux:10"),
        ("AlmaLinux 10", "almalinux:10"), ("openSUSE Tumbleweed", "opensuse:tumbleweed"),
    ]

    /// The chosen user data, read when creating.
    private var userData: String? { cloudInit.flatMap { try? String(contentsOf: $0, encoding: .utf8) } }

    private func chooseCloudInit() {
        let panel = NSOpenPanel()
        panel.message = "cloud-init user data (#cloud-config)"
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }
        cloudInit = url
        let text = (try? String(contentsOf: url, encoding: .utf8)) ?? ""
        cloudInitError = text.hasPrefix("#cloud-config") || text.hasPrefix("#!") ? nil : "Starts with #cloud-config (or #! for a script)."
    }

    private var validName: Bool {
        (1...12).contains(name.count) && name.allSatisfy { $0.isLowercase || $0.isNumber || $0 == "-" } && !name.hasPrefix("-")
    }

    var body: some View {
        Form {
            TextField("Name", text: $name, prompt: Text("e.g. dev"))
            Picker("Distribution", selection: $distro) {
                ForEach(distros, id: \.1) { Text($0.0).tag($0.1) }
            }
            Picker("Architecture", selection: $arch) {
                Text("Apple silicon (arm64)").tag("arm64")
                Text("Intel (amd64, with Rosetta)").tag("amd64")
            }
            LabeledContent("cloud-init") {
                HStack {
                    Text(cloudInit?.lastPathComponent ?? "None").foregroundStyle(.secondary)
                    if cloudInit != nil { Button("Clear") { cloudInit = nil; cloudInitError = nil } }
                    Button("Choose…") { chooseCloudInit() }
                }
            }
            if let cloudInitError { Text(cloudInitError).font(.caption).foregroundStyle(.red) }
            Text("Your user, sudo, and your Mac home folder are set up inside. Reachable at \(name.isEmpty ? "<name>" : name).berth.local; `ssh \(name.isEmpty ? "<name>" : name).berth` too.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
        .frame(width: 420)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Create") { create(name, distro, arch, userData ?? ""); dismiss() }
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
