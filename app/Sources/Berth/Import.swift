import SwiftUI

/// `berth import`: what another engine (running) has, then copying it.
struct ImportSheet: View {
    @Environment(\.dismiss) private var dismiss
    @State private var source = "orbstack"
    @State private var project = ""
    @State private var output = ""
    @State private var running = false
    @State private var previewed = false

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
                Button("Close") { dismiss() }.keyboardShortcut(.cancelAction).disabled(running)
                Button("Preview") { run(dryRun: true) }.disabled(running)
                Button("Import") { run(dryRun: false) }.keyboardShortcut(.defaultAction).disabled(running || !previewed)
            }
        }
        .padding()
        .frame(width: 560)
    }

    private func run(dryRun: Bool) {
        var args = [BerthPaths.cli, "import", source]
        let name = project.trimmingCharacters(in: .whitespaces)
        if !name.isEmpty { args += ["--project", name] }
        if dryRun { args.append("--dry-run") }
        running = true
        output = dryRun ? "" : output + "\n— importing —\n"
        Task {
            let result = await Task.detached { Engine.run(args) }.value
            output += result.output
            if dryRun { previewed = result.code == 0 }
            running = false
        }
    }
}
