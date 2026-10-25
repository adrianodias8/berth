import ServiceManagement
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var engine: Engine
    @Environment(\.openWindow) private var openWindow
    @State private var config = BerthConfig.load()
    @State private var saved = BerthConfig.load()
    @State private var launchAtLogin = SMAppService.mainApp.status == .enabled
    @State private var showImport = UserDefaults.standard.string(forKey: "openSheet") == "Import"
    /// "auto", "none" or "url"; the URL itself lives in `proxyURL` while
    /// it's being typed.
    @State private var proxyMode = { () -> String in
        let p = BerthConfig.load().proxy ?? "auto"
        return ["auto", "none"].contains(p) ? p : "url"
    }()
    @State private var proxyURL = { () -> String in
        let p = BerthConfig.load().proxy ?? ""
        return p.hasPrefix("http") ? p : ""
    }()

    /// http(s)://host[:port], as `berth config set proxy` accepts.
    private var proxyURLValid: Bool {
        guard let url = URL(string: proxyURL), let scheme = url.scheme, ["http", "https"].contains(scheme) else { return false }
        return url.host?.isEmpty == false
    }
    @AppStorage("autoStart") private var autoStart = true
    @State private var helperMessage: String?
    @State private var httpsTrusted = TLSTrust.cached
    @State private var httpsMessage: String?
    @State private var engines: [String: Bool] = [:]
    @State private var engineMessage: String?

    private let hostCPUs = ProcessInfo.processInfo.activeProcessorCount
    private let hostMemoryGiB = Int(ProcessInfo.processInfo.physicalMemory >> 30)

    var body: some View {
        Form {
            SwiftUI.Section("Resources") {
                Stepper(value: cpusBinding, in: 1...hostCPUs) {
                    LabeledContent("CPUs", value: "\(config.cpus ?? engine.status?.cpus ?? hostCPUs)")
                }
                VStack(alignment: .leading) {
                    LabeledContent("Memory limit", value: "\(memoryGiB) GiB")
                    Slider(value: memoryBinding, in: 2...Double(max(hostMemoryGiB - 2, 4)), step: 1)
                    Text("A ceiling, not a reservation: the VM only takes what it uses. What it has used stays with it until it restarts.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if ProcessInfo.processInfo.operatingSystemVersion.majorVersion >= 27 {
                    Picker("File sharing", selection: Binding(get: { config.fileSharing ?? "berth" }, set: { config.fileSharing = $0 })) {
                        Text("berth (faster)").tag("berth")
                        Text("Apple's virtiofs").tag("apple")
                    }
                }
                VStack(alignment: .leading) {
                    Toggle("IPv6 for containers", isOn: Binding(get: { config.ipv6 ?? false }, set: { config.ipv6 = $0 ? true : nil }))
                    Text("Docker networks get IPv6 subnets. Internet access over IPv6 needs IPv6 on this Mac.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                VStack(alignment: .leading) {
                    Picker("Proxy for Docker", selection: $proxyMode) {
                        Text("The Mac's settings").tag("auto")
                        Text("None").tag("none")
                        Text("URL").tag("url")
                    }
                    .onChange(of: proxyMode) { _, _ in syncProxy() }
                    if proxyMode == "url" {
                        TextField("Proxy URL", text: $proxyURL, prompt: Text("http://host:port"))
                            .textFieldStyle(.roundedBorder)
                            .onChange(of: proxyURL) { _, _ in syncProxy() }
                        if !proxyURL.isEmpty && !proxyURLValid {
                            Text("An http:// or https:// URL with a host.").font(.caption).foregroundStyle(.red)
                        }
                    }
                    Text("Used by Docker for pulls and pushes; applies when Berth next starts.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                VStack(alignment: .leading) {
                    Toggle("Published ports only on this Mac", isOn: Binding(get: { config.ports == "localhost" }, set: { config.ports = $0 ? "localhost" : nil }))
                    Text("`-p 8080:80` listens on 127.0.0.1, not on your network.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                VStack(alignment: .leading) {
                    Toggle("Files in Finder (~/Berth)", isOn: Binding(get: { config.finder ?? true }, set: { config.finder = $0 ? nil : false }))
                    Text("Volumes, machines and running containers' files, at ~/Berth while Berth runs.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if config != saved {
                    HStack {
                        Text("Restart Berth to apply.").foregroundStyle(.secondary)
                        Spacer()
                        Button("Save") { save(restart: false) }.disabled(proxyMode == "url" && !proxyURLValid)
                        Button("Save & Restart") { save(restart: true) }.disabled(!engine.isRunning || (proxyMode == "url" && !proxyURLValid))
                    }
                }
            }

            SwiftUI.Section("Moving over") {
                LabeledContent("Images, volumes and containers from OrbStack or Docker Desktop") {
                    Button("Import…") { showImport = true }.disabled(!engine.isRunning)
                }
            }

            SwiftUI.Section("Startup") {
                Toggle("Start Berth when the app opens", isOn: $autoStart)
                Toggle("Keep containers running across stops (resume on start)", isOn: Binding(
                    get: { config.stop == "suspend" },
                    set: { setStop($0 ? "suspend" : nil) }
                ))
                Toggle("Open at login", isOn: $launchAtLogin)
                    .onChange(of: launchAtLogin) { _, on in
                        do {
                            if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
                        } catch {
                            engine.lastError = "Open at login: \(error.localizedDescription)"
                            launchAtLogin = SMAppService.mainApp.status == .enabled
                        }
                    }
            }

            SwiftUI.Section("Networking") {
                LabeledContent("Container domains", value: "*.berth.local")
                LabeledContent("Host from containers", value: "host.docker.internal")
                LabeledContent("Container IPs", value: engine.status?.containerIps ?? "—")
                HStack {
                    Text("Direct container IPs use a small root helper that manages routes for Docker subnets.")
                        .font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    Button(engine.status?.helperOutdated == true ? "Update Helper…" : helperInstalled ? "Reinstall Helper…" : "Install Helper…") { installHelper() }
                }
                if let helperMessage { Text(helperMessage).font(.caption).foregroundStyle(.secondary) }
            }

            SwiftUI.Section("Container engines") {
                LabeledContent("Docker", value: "Always on")
                Toggle("Podman", isOn: engineBinding("podman"))
                Toggle("nerdctl (containerd)", isOn: engineBinding("nerdctl"))
                Text(engineMessage ?? "Podman: `podman` on your Mac uses berth. nerdctl: `nerdctl` runs in the VM. Containers from every engine get ports, domains and IPs.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .task { await loadEngines() }

            SwiftUI.Section("HTTPS") {
                LabeledContent("Container domains over HTTPS", value: httpsTrusted ? "On" : "Certificate not trusted")
                HStack {
                    Text("berth's local certificate authority only covers .local names. Trusting it asks for your password.")
                        .font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    if httpsTrusted {
                        Button("Remove Trust") { runTLS("untrust") }
                    } else {
                        Button("Trust Certificate…") { runTLS("trust") }
                    }
                }
                if let httpsMessage { Text(httpsMessage).font(.caption).foregroundStyle(.secondary) }
            }

            SwiftUI.Section("Setup") {
                HStack {
                    Text("Run the first-run setup again (engine, networking, HTTPS).")
                        .font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    Button("Run Setup…") {
                        UserDefaults.standard.set(false, forKey: "onboarded")
                        openWindow(id: "main")
                        NSApp.activate(ignoringOtherApps: true)
                    }
                }
            }

            SwiftUI.Section("Docker") {
                LabeledContent("Socket") { Text(BerthPaths.dockerSocket).textSelection(.enabled).font(.caption.monospaced()) }
                LabeledContent("CLI") { Text("docker context use berth").textSelection(.enabled).font(.caption.monospaced()) }
                HStack {
                    Button("Show Data in Finder") { NSWorkspace.shared.open(BerthPaths.root) }
                    Button("Open Daemon Log") { NSWorkspace.shared.open(BerthPaths.daemonLog) }
                }
            }
        }
        .formStyle(.grouped)
        .sheet(isPresented: $showImport) { ImportSheet() }
        .frame(width: 520, height: 720)
    }

    private func loadEngines() async {
        let out = await offPool { Engine.run([BerthPaths.cli, "engine", "status"]).output }
        var states: [String: Bool] = [:]
        for line in out.split(separator: "\n") {
            let parts = line.split(separator: " ", maxSplits: 1)
            if parts.count == 2 { states[String(parts[0])] = parts[1].trimmingCharacters(in: .whitespaces).hasPrefix("enabled") }
        }
        engines = states
    }

    private func engineBinding(_ name: String) -> Binding<Bool> {
        Binding(get: { engines[name] ?? false }, set: { on in
            engines[name] = on
            engineMessage = on ? "Setting up \(name)… (the first time installs it in the VM)" : nil
            Task {
                let result = await offPool { Engine.run([BerthPaths.cli, "engine", on ? "enable" : "disable", name]) }
                engineMessage = result.code == 0 ? nil : result.output.trimmingCharacters(in: .whitespacesAndNewlines)
                await loadEngines()
            }
        })
    }

    /// `berth tls trust|untrust`; macOS shows its own password prompt.
    private func runTLS(_ action: String) {
        httpsMessage = action == "trust" ? "Waiting for authorization…" : nil
        Task.detached {
            let (result, trusted) = await offPool { (Engine.run([BerthPaths.cli, "tls", action]), TLSTrust.check()) }
            await MainActor.run {
                httpsTrusted = trusted
                httpsMessage = result.code == 0 ? nil : result.output.trimmingCharacters(in: .whitespacesAndNewlines)
            }
        }
    }

    private var helperInstalled: Bool { Setup.helperInstalled }

    private var cpusBinding: Binding<Int> {
        Binding(get: { config.cpus ?? engine.status?.cpus ?? hostCPUs }, set: { config.cpus = $0 })
    }

    private var memoryGiB: Int {
        config.memoryMib.map { $0 / 1024 } ?? Int((engine.status?.memory ?? 8 << 30) >> 30)
    }

    private var memoryBinding: Binding<Double> {
        Binding(get: { Double(memoryGiB) }, set: { config.memoryMib = Int($0) * 1024 })
    }

    private func syncProxy() {
        switch proxyMode {
        case "auto": config.proxy = nil
        case "none": config.proxy = "none"
        default: config.proxy = proxyURL
        }
    }

    /// The suspend toggle applies at once and alone: other edits stay pending.
    private func setStop(_ value: String?) {
        var updated = saved
        updated.stop = value
        do {
            try updated.save(over: saved)
            saved.stop = value
            config.stop = value
        } catch {
            engine.lastError = "Saving settings: \(error.localizedDescription)"
        }
    }

    private func save(restart: Bool) {
        do {
            try config.save(over: saved)
            saved = config
            if restart { engine.restart() }
        } catch {
            engine.lastError = "Saving settings: \(error.localizedDescription)"
        }
    }

    private func installHelper() {
        helperMessage = "Waiting for authorization…"
        Task {
            switch await Setup.installPrivileged(force: true) {
            case .done: helperMessage = "Installed."
            case .skipped: helperMessage = nil
            case let .failed(message): helperMessage = message
            default: break
            }
        }
    }
}
