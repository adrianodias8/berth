import AppKit
import SwiftUI

@main
struct BerthApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var engine = Engine()

    var body: some Scene {
        Window("Berth", id: "main") {
            MainWindow()
                .environmentObject(engine)
                .frame(minWidth: 900, minHeight: 560)
        }
        .defaultSize(width: 1100, height: 700)
        .commands {
            CommandGroup(after: .appInfo) {
                Button(engine.isRunning ? "Stop Berth" : "Start Berth") {
                    engine.isRunning ? engine.stop() : engine.start()
                }
                .keyboardShortcut("s", modifiers: [.command, .shift])
            }
        }

        MenuBarExtra {
            MenuBarContent()
                .environmentObject(engine)
        } label: {
            Image(systemName: engine.isRunning ? "ferry.fill" : "ferry")
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView()
                .environmentObject(engine)
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Keep running in the menu bar after the window closes, like OrbStack.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

struct MenuBarContent: View {
    @EnvironmentObject var engine: Engine
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Circle().fill(engine.state.color).frame(width: 8, height: 8)
                Text("Berth").font(.headline)
                Text(engine.state.label).foregroundStyle(.secondary)
                Spacer()
                if engine.isRunning {
                    Button("Stop") { engine.stop() }
                } else {
                    Button("Start") { engine.start() }
                        .disabled(engine.state == .starting || engine.state == .stopping)
                }
            }

            if engine.isRunning {
                Divider()
                let running = engine.containers.filter(\.isRunning)
                if engine.containers.isEmpty {
                    Text("No containers").foregroundStyle(.secondary)
                } else {
                    Text("\(running.count) of \(engine.containers.count) containers running")
                        .font(.caption).foregroundStyle(.secondary)
                    // An explicit height: a ScrollView has no intrinsic height and
                    // collapses to nothing inside a menu-bar popover.
                    ScrollView {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(engine.containers) { c in
                                MenuContainerRow(container: c).frame(height: 22)
                            }
                        }
                    }
                    .frame(height: min(CGFloat(engine.containers.count) * 26, 260))
                }
            }

            if let error = engine.lastError {
                Text(error).font(.caption).foregroundStyle(.red).lineLimit(3)
            }

            Divider()
            HStack {
                Button("Open Berth") {
                    openWindow(id: "main")
                    NSApp.activate(ignoringOtherApps: true)
                }
                Spacer()
                SettingsLink { Text("Settings…") }
                Button("Quit") { NSApp.terminate(nil) }
            }
        }
        .padding(12)
        .frame(width: 320)
    }
}

struct MenuContainerRow: View {
    @EnvironmentObject var engine: Engine
    let container: Container

    var body: some View {
        HStack {
            Circle().fill(container.isRunning ? .green : .gray).frame(width: 6, height: 6)
            Text(container.name).lineLimit(1)
            Spacer()
            if container.isRunning {
                if let url = webURL(for: container) {
                    Button { NSWorkspace.shared.open(url) } label: { Image(systemName: "safari") }
                        .buttonStyle(.borderless).help("Open \(url.absoluteString)")
                }
                Button { engine.perform("Stop") { try await Docker.stop(container.id) } } label: {
                    Image(systemName: "stop.fill")
                }
                .buttonStyle(.borderless).help("Stop")
            } else {
                Button { engine.perform("Start") { try await Docker.start(container.id) } } label: {
                    Image(systemName: "play.fill")
                }
                .buttonStyle(.borderless).help("Start")
            }
        }
        .font(.callout)
    }
}

/// Where a running container's web UI most likely is: its domain if it
/// exposes port 80, else the first published port on localhost.
func webURL(for container: Container) -> URL? {
    if (container.ports ?? []).contains(where: { $0.privatePort == 80 && $0.type == "tcp" }) {
        return URL(string: "http://\(container.domain)")
    }
    if let port = container.publishedPorts.first?.publicPort {
        return URL(string: "http://localhost:\(port)")
    }
    return nil
}
