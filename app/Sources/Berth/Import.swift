import SwiftUI

/// `berth import`: what another engine (running) has, then copying it.
struct ImportSheet: View {
    @Environment(\.dismiss) private var dismiss
    @State private var source = "orbstack"
    @State private var project = ""
    @State private var output = ""
    @State private var running = false
    @State private var previewed = false
    @State private var process: Process?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Import from another engine").font(.headline)
            Text("Copies images, volumes (contents, owners and modes), networks and containers. The other engine must be running; nothing in it is changed. Containers arrive stopped: start them, or `docker compose up -d` in their project.")
                .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            Picker("From", selection: $source) {
                Text("OrbStack").tag("orbstack")
                Text("Docker Desktop").tag("docker-desktop")
            }
            .onChange(of: source) { _, _ in previewed = false; output = "" }
            TextField("Only one Compose project (optional)", text: $project)
                .onChange(of: project) { _, _ in previewed = false }
            ScrollView {
                Text(output.isEmpty ? "Preview first to see what would come over." : output)
                    .font(.system(.caption, design: .monospaced))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .textSelection(.enabled)
            }
            .frame(height: 220)
            .padding(6)
            .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
            HStack {
                if running { ProgressView().controlSize(.small) }
                Spacer()
                if running { Button("Stop") { process?.terminate() } }
                Button("Close") { dismiss() }.keyboardShortcut(.cancelAction).disabled(running)
                Button("Preview") { run(dryRun: true) }.disabled(running)
                Button("Import") { run(dryRun: false) }.keyboardShortcut(.defaultAction).disabled(running || !previewed)
            }
        }
        .padding()
        .frame(width: 480)
    }

    /// The CLI, its output shown as it comes (an import can take a while).
    private func run(dryRun: Bool) {
        var args = ["import", source]
        let name = project.trimmingCharacters(in: .whitespaces)
        if !name.isEmpty { args += ["--project", name] }
        if dryRun { args.append("--dry-run") }
        output = dryRun ? "" : output + "\n— importing —\n"
        // Preview again before another import.
        previewed = false
        let p = Process()
        p.executableURL = URL(fileURLWithPath: BerthPaths.cli)
        p.arguments = args
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        pipe.fileHandleForReading.readabilityHandler = { handle in
            let text = String(decoding: handle.availableData, as: UTF8.self)
            if !text.isEmpty { Task { @MainActor in output += text } }
        }
        p.terminationHandler = { done in
            let ok = done.terminationStatus == 0
            Task { @MainActor in
                pipe.fileHandleForReading.readabilityHandler = nil
                if let rest = try? pipe.fileHandleForReading.readToEnd() { output += String(decoding: rest, as: UTF8.self) }
                previewed = dryRun && ok
                running = false
                process = nil
            }
        }
        do {
            try p.run()
            process = p
            running = true
        } catch {
            output += "could not run berth: \(error.localizedDescription)\n"
        }
    }
}
