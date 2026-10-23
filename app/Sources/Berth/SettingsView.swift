import ServiceManagement
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var engine: Engine
    @State private var config = BerthConfig.load()
    @State private var saved = BerthConfig.load()
    @State private var launchAtLogin = SMAppService.mainApp.status == .enabled
    @AppStorage("autoStart") private var autoStart = true
    @State private var helperMessage: String?

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
                    Button(helperInstalled ? "Reinstall Helper…" : "Install Helper…") { installHelper() }
                }
                if let helperMessage { Text(helperMessage).font(.caption).foregroundStyle(.secondary) }
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
        .frame(width: 520, height: 560)
    }

    private var helperInstalled: Bool { FileManager.default.fileExists(atPath: "/Library/LaunchDaemons/dev.berth.helper.plist") }

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

    /// The helper must be installed as root; macOS shows its admin prompt.
    private func installHelper() {
        let cli = BerthPaths.cli.replacingOccurrences(of: "'", with: "'\\''")
        let script = "do shell script \"SUDO_UID=\(getuid()) '\(cli)' helper install\" with administrator privileges "
            + "with prompt \"Berth needs to install a helper so container IPs are reachable from your Mac.\""
        // NSAppleScript belongs on the main thread; the admin prompt is modal anyway.
        var error: NSDictionary?
        let output = NSAppleScript(source: script)?.executeAndReturnError(&error).stringValue
        if let error {
            helperMessage = (error[NSAppleScript.errorMessage] as? String) ?? "Install failed"
        } else {
            helperMessage = output ?? "Installed."
        }
    }
}
