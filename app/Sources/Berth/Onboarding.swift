import AppKit
import SwiftUI

/// First-run setup: everything a user would otherwise do on the command line,
/// done in order with the system's own prompts. Shown until it completes once.
struct OnboardingView: View {
    @EnvironmentObject var engine: Engine
    @Environment(\.dismiss) private var dismiss
    @AppStorage("onboarded") private var onboarded = false
    @State private var steps: [Step] = Step.all
    @State private var running = false

    struct Step: Identifiable {
        enum Kind { case engine, privileged, https }
        enum State: Equatable { case pending, running, done, skipped, failed(String) }

        let kind: Kind
        let title: String
        let detail: String
        var state: State = .pending
        var id: Kind { kind }

        static let all = [
            Step(kind: .engine, title: "Start the Linux engine",
                 detail: "Downloads Debian and installs Docker the first time (a few minutes)."),
            Step(kind: .privileged, title: "Set up networking",
                 detail: "Container IPs, the default Docker socket and the berth command. macOS asks for an administrator password once."),
            Step(kind: .https, title: "Enable HTTPS for containers",
                 detail: "Trusts berth's certificate, limited to .local names. macOS asks for your password."),
        ]
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(spacing: 14) {
                Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 56, height: 56)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Welcome to Berth").font(.title.weight(.semibold))
                    Text("Docker containers on your Mac, set up in one go.").foregroundStyle(.secondary)
                }
            }

            VStack(alignment: .leading, spacing: 14) {
                ForEach(steps) { step in
                    HStack(alignment: .top, spacing: 12) {
                        StepIcon(state: step.state).frame(width: 22)
                        VStack(alignment: .leading, spacing: 3) {
                            Text(step.title).fontWeight(.medium)
                            Text(step.detail).font(.callout).foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                            if case let .failed(message) = step.state {
                                Text(message).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                            }
                        }
                    }
                }
            }
            .padding(14)
            .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 10))

            HStack {
                if !running && !finished {
                    Button("Skip for Now") { finish() }
                }
                Spacer()
                if finished {
                    Button("Get Started") { finish() }.keyboardShortcut(.defaultAction).controlSize(.large)
                } else {
                    Button(anyFailed ? "Try Again" : "Set Up Berth") { Task { await runAll() } }
                        .keyboardShortcut(.defaultAction).controlSize(.large).disabled(running)
                }
            }
        }
        .padding(24)
        .frame(width: 520)
        .interactiveDismissDisabled(running)
    }

    private var finished: Bool { steps.allSatisfy { $0.state == .done || $0.state == .skipped } }
    private var anyFailed: Bool { steps.contains { if case .failed = $0.state { true } else { false } } }

    private func finish() {
        onboarded = true
        dismiss()
    }

    private func set(_ kind: Step.Kind, _ state: Step.State) {
        if let i = steps.firstIndex(where: { $0.kind == kind }) { steps[i].state = state }
    }

    private func runAll() async {
        running = true
        defer { running = false }
        for step in steps where step.state != .done {
            set(step.kind, .running)
            let result: Step.State
            switch step.kind {
            case .engine: result = await startEngine()
            case .privileged: result = Setup.installPrivileged()
            case .https: result = await Setup.trustCertificate()
            }
            set(step.kind, result)
            // Networking and HTTPS are independent of each other, but both
            // need the engine (helper routes, the CA it creates).
            if step.kind == .engine, case .failed = result { return }
        }
    }

    private func startEngine() async -> Step.State {
        if !engine.isRunning { engine.start() }
        for _ in 0..<2400 {
            switch engine.state {
            case .running: return .done
            case let .failed(message): return .failed(message)
            default: try? await Task.sleep(for: .milliseconds(500))
            }
        }
        return .failed("Timed out waiting for the engine. See ~/.berth/log/daemon.log.")
    }
}

private struct StepIcon: View {
    let state: OnboardingView.Step.State

    var body: some View {
        switch state {
        case .pending: Image(systemName: "circle").foregroundStyle(.tertiary)
        case .running: ProgressView().controlSize(.small)
        case .done: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        case .skipped: Image(systemName: "minus.circle").foregroundStyle(.secondary)
        case .failed: Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.red)
        }
    }
}

/// Privileged and keychain setup, shared by onboarding and Settings.
enum Setup {
    static var helperInstalled: Bool {
        FileManager.default.fileExists(atPath: "/Library/LaunchDaemons/dev.berth.helper.plist")
    }

    /// One administrator prompt: the route/socket helper and the `berth` CLI
    /// link in /usr/local/bin. NSAppleScript runs on the main thread; the
    /// prompt is modal anyway.
    @MainActor
    static func installPrivileged() -> OnboardingView.Step.State {
        let cli = shellQuote(BerthPaths.cli)
        let command = "SUDO_UID=\(getuid()) \(cli) helper install"
            + " && mkdir -p /usr/local/bin && ln -sf \(cli) /usr/local/bin/berth"
        let script = "do shell script \"\(command.replacingOccurrences(of: "\"", with: "\\\""))\" "
            + "with administrator privileges with prompt \"Berth needs to set up networking for containers.\""
        var error: NSDictionary?
        NSAppleScript(source: script)?.executeAndReturnError(&error)
        if let error {
            let code = error[NSAppleScript.errorNumber] as? Int
            return code == -128 ? .skipped : .failed((error[NSAppleScript.errorMessage] as? String) ?? "Install failed")
        }
        return .done
    }

    /// `berth tls trust`; macOS shows its own password prompt for trust changes.
    static func trustCertificate() async -> OnboardingView.Step.State {
        if TLSTrust.check() { return .done }
        let result = await Task.detached { Engine.run([BerthPaths.cli, "tls", "trust"]) }.value
        if TLSTrust.check() { return .done }
        return result.code == 0 ? .skipped : .failed(result.output.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private static func shellQuote(_ s: String) -> String { "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'" }
}
