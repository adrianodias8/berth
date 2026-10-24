import SwiftUI

// MARK: - Snapshots

/// One `~/.berth/snapshots/<name>/meta.json` (src/snapshots.rs).
struct Snapshot: Decodable, Identifiable, Hashable {
    let name: String
    let created: Double
    let with_memory: Bool
    var id: String { name }
    var date: Date { Date(timeIntervalSince1970: created) }
}

/// Snapshots are files berth keeps; creating, restoring and deleting go
/// through the CLI (restoring stops and starts the VM, which the daemon
/// can't do to itself).
enum Snapshots {
    static var directory: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".berth/snapshots")
    }

    static func list() -> [Snapshot] {
        let dirs = (try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? []
        return dirs
            .filter { !$0.lastPathComponent.hasPrefix(".") }
            .compactMap { try? JSONDecoder().decode(Snapshot.self, from: Data(contentsOf: $0.appendingPathComponent("meta.json"))) }
            .sorted { $0.created > $1.created }
    }

    /// Run `berth snapshot …` off the main thread; the error output if it failed.
    static func run(_ args: [String]) async -> String? {
        let result = await Task.detached { Engine.run([BerthPaths.cli, "snapshot"] + args) }.value
        return result.code == 0 ? nil : result.output.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

struct SnapshotsView: View {
    @EnvironmentObject var engine: Engine
    @State private var snapshots: [Snapshot] = []
    @State private var selection: String?
    @State private var showCreate = false
    @State private var busy: String?
    @State private var error: String?
    @State private var confirmRestore: Snapshot?
    @State private var confirmDelete: Snapshot?

    var body: some View {
        VStack(spacing: 0) {
            if snapshots.isEmpty {
                ContentUnavailableView {
                    Label("No Snapshots", systemImage: "clock.arrow.circlepath")
                } description: {
                    Text("A snapshot saves the whole VM: containers, volumes, images, machines and, while the Mac is unlocked, running processes. Take one before a risky change, then go back to it.")
                } actions: {
                    Button("Take Snapshot…") { showCreate = true }.disabled(busy != nil)
                }
            } else {
                List(selection: $selection) {
                    ForEach(snapshots) { s in
                        HStack {
                            Image(systemName: s.with_memory ? "memorychip" : "internaldrive")
                                .foregroundStyle(.secondary)
                                .frame(width: 20)
                            VStack(alignment: .leading, spacing: 1) {
                                Text(s.name).fontWeight(.medium)
                                Text("\(s.date.formatted(date: .abbreviated, time: .shortened)) · \(s.with_memory ? "memory and disk" : "disk")")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            Spacer()
                            if busy == s.name { ProgressView().controlSize(.small) }
                        }
                        .tag(s.name)
                        .contextMenu {
                            Button("Restore…") { confirmRestore = s }.disabled(busy != nil)
                            Divider()
                            Button("Delete…", role: .destructive) { confirmDelete = s }.disabled(busy != nil)
                        }
                    }
                }
            }
            if let error {
                Text(error).font(.caption).foregroundStyle(.red).padding(8).frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .navigationTitle("Snapshots")
        .toolbar {
            ToolbarItem {
                Button { showCreate = true } label: { Label("Take Snapshot", systemImage: "plus") }
                    .disabled(busy != nil)
            }
            ToolbarItem {
                Button { if let s = snapshots.first(where: { $0.name == selection }) { confirmRestore = s } } label: {
                    Label("Restore", systemImage: "clock.arrow.circlepath")
                }
                .disabled(selection == nil || busy != nil)
            }
        }
        .sheet(isPresented: $showCreate) {
            CreateSnapshotSheet { name, diskOnly in
                showCreate = false
                perform(name, ["create", name] + (diskOnly ? ["--disk-only"] : []))
            }
        }
        .confirmationDialog(
            "Restore \(confirmRestore?.name ?? "")?",
            isPresented: Binding(get: { confirmRestore != nil }, set: { if !$0 { confirmRestore = nil } }),
            presenting: confirmRestore
        ) { s in
            Button("Restore", role: .destructive) { perform(s.name, ["restore", s.name, "--yes"]) }
        } message: { s in
            Text("The VM goes back to how it was \(s.date.formatted(.relative(presentation: .named))). Everything in it since then is lost. Files on your Mac aren't affected.")
        }
        .confirmationDialog(
            "Delete \(confirmDelete?.name ?? "")?",
            isPresented: Binding(get: { confirmDelete != nil }, set: { if !$0 { confirmDelete = nil } }),
            presenting: confirmDelete
        ) { s in
            Button("Delete", role: .destructive) { perform(s.name, ["delete", s.name]) }
        }
        .task {
            // Snapshots also come and go from the CLI.
            while !Task.isCancelled {
                if busy == nil { reload() }
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    private func reload() { snapshots = Snapshots.list() }

    private func perform(_ name: String, _ args: [String]) {
        busy = name
        error = nil
        Task {
            error = await Snapshots.run(args)
            busy = nil
            reload()
        }
    }
}

struct CreateSnapshotSheet: View {
    var onCreate: (String, Bool) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var name = "snapshot-" + Date().formatted(.iso8601.year().month().day().dateSeparator(.dash)) + "-" +
        Date().formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits))
    @State private var diskOnly = false

    private var valid: Bool {
        !name.isEmpty && name.count <= 64 && !name.hasPrefix(".")
            && name.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber || "-_.".contains($0)) }
    }

    var body: some View {
        Form {
            TextField("Name", text: $name)
            Toggle("Disk only (don't save running processes)", isOn: $diskOnly)
            Text("Instant and copy-on-write: it only takes space as the VM changes. Bind-mounted files on your Mac aren't included.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
        .frame(width: 420)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            ToolbarItem(placement: .confirmationAction) {
                Button("Take Snapshot") { onCreate(name, diskOnly) }.disabled(!valid)
            }
        }
    }
}
