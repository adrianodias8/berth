import ServiceManagement
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var engine: Engine
    @Environment(\.openWindow) private var openWindow
    @State private var config = BerthConfig.load()
    @State private var saved = BerthConfig.load()
    @State private var launchAtLogin = SMAppService.mainApp.status == .enabled
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
                    Text("A ceiling, not a reservation: the VM returns memory it isn't using.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if config != saved {
                    HStack {
                        Text("Restart Berth to apply.").foregroundStyle(.secondary)
                        Spacer()
                        Button("Save") { save(restart: false) }
                        Button("Save & Restart") { save(restart: true) }.disabled(!engine.isRunning)
                    }
                }
            }

            SwiftUI.Section("Startup") {
                Toggle("Start Berth when the app opens", isOn: $autoStart)
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
        .frame(width: 520, height: 720)
    }

    private func loadEngines() async {
        let out = await Task.detached { Engine.run([BerthPaths.cli, "engine", "status"]).output }.value
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
                let result = await Task.detached { Engine.run([BerthPaths.cli, "engine", on ? "enable" : "disable", name]) }.value
                engineMessage = result.code == 0 ? nil : result.output.trimmingCharacters(in: .whitespacesAndNewlines)
                await loadEngines()
            }
        })
    }

    /// `berth tls trust|untrust`; macOS shows its own password prompt.
    private func runTLS(_ action: String) {
        httpsMessage = action == "trust" ? "Waiting for authorization…" : nil
        Task.detached {
            let result = Engine.run([BerthPaths.cli, "tls", action])
            let trusted = TLSTrust.check()
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

    private func save(restart: Bool) {
        do {
            try config.save()
            saved = config
            if restart { engine.restart() }
        } catch {
            engine.lastError = "Saving settings: \(error.localizedDescription)"
        }
    }

    private func installHelper() {
        helperMessage = "Waiting for authorization…"
        switch Setup.installPrivileged(force: true) {
        case .done: helperMessage = "Installed."
        case .skipped: helperMessage = nil
        case let .failed(message): helperMessage = message
        default: break
        }
    }
}
