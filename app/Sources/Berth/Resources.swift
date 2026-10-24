import SwiftUI
import UniformTypeIdentifiers

// MARK: - Files

struct FilesView: View {
    let containerID: String
    @State private var path = "/"
    @State private var entries: [FileEntry] = []
    @State private var error: String?
    @State private var loading = false
    @State private var selection: FileEntry.ID?
    @State private var preview: (name: String, text: String)?

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 6) {
                Button { navigate(to: parent(of: path)) } label: { Image(systemName: "chevron.up") }
                    .disabled(path == "/")
                    .help("Parent folder")
                PathBar(path: path) { navigate(to: $0) }
                Spacer()
                if loading { ProgressView().controlSize(.small) }
                Button { load() } label: { Image(systemName: "arrow.clockwise") }.help("Reload")
            }
            .buttonStyle(.borderless)
            .padding(8)
            Divider()
            if let error {
                EmptyHint(icon: "exclamationmark.triangle", text: error)
            } else {
                Table(entries, selection: $selection) {
                    TableColumn("Name") { e in
                        Label(e.name, systemImage: e.isDirectory ? "folder.fill" : e.isLink ? "arrow.turn.up.right" : "doc")
                    }
                    TableColumn("Size") { e in Text(e.isDirectory ? "—" : formatBytes(e.size)).monospacedDigit() }
                        .width(80)
                    TableColumn("Modified") { e in Text(e.modified.formatted(date: .abbreviated, time: .shortened)) }
                        .width(150)
                }
                .contextMenu(forSelectionType: FileEntry.ID.self) { ids in
                    if let name = ids.first, let entry = entries.first(where: { $0.id == name }), !entry.isDirectory {
                        Button("View") { view(entry) }
                        Button("Download…") { download(entry) }
                    }
                    if let name = ids.first { Button("Copy Path") { copy(join(path, name)) } }
                } primaryAction: { ids in
                    guard let name = ids.first, let entry = entries.first(where: { $0.id == name }) else { return }
                    if entry.isDirectory || entry.isLink { navigate(to: join(path, entry.name)) } else { view(entry) }
                }
            }
        }
        .task(id: containerID) { load() }
        .sheet(isPresented: Binding(get: { preview != nil }, set: { if !$0 { preview = nil } })) {
            if let preview {
                VStack(alignment: .leading) {
                    HStack {
                        Text(preview.name).font(.headline)
                        Spacer()
                        Button("Close") { self.preview = nil }.keyboardShortcut(.cancelAction)
                    }
                    ScrollView {
                        Text(preview.text).font(.system(.caption, design: .monospaced))
                            .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                .padding()
                .frame(width: 700, height: 500)
            }
        }
    }

    private func navigate(to newPath: String) {
        path = newPath
        load()
    }

    private func load() {
        loading = true
        let target = path
        Task {
            do {
                let list = try await Docker.listFiles(containerID, path: target)
                if target == path { entries = list; error = nil }
            } catch {
                if target == path { self.error = error.localizedDescription; entries = [] }
            }
            loading = false
        }
    }

    private func view(_ entry: FileEntry) {
        Task {
            do {
                let data = try await Docker.readFile(containerID, path: join(path, entry.name))
                let limited = data.prefix(512 * 1024)
                let text = String(data: limited, encoding: .utf8) ?? "(binary file, \(formatBytes(Int64(data.count))))"
                preview = (entry.name, text)
            } catch {
                self.error = error.localizedDescription
            }
        }
    }

    private func download(_ entry: FileEntry) {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = entry.name
        guard panel.runModal() == .OK, let url = panel.url else { return }
        Task {
            do {
                try await Docker.readFile(containerID, path: join(path, entry.name)).write(to: url)
            } catch {
                self.error = error.localizedDescription
            }
        }
    }

    private func join(_ dir: String, _ name: String) -> String { dir == "/" ? "/\(name)" : "\(dir)/\(name)" }

    private func parent(of p: String) -> String {
        let parent = (p as NSString).deletingLastPathComponent
        return parent.isEmpty ? "/" : parent
    }
}

struct PathBar: View {
    let path: String
    let go: (String) -> Void

    var body: some View {
        let parts = path.split(separator: "/").map(String.init)
        HStack(spacing: 2) {
            Button("/") { go("/") }
            ForEach(parts.indices, id: \.self) { i in
                Image(systemName: "chevron.right").font(.caption2).foregroundStyle(.tertiary)
                Button(parts[i]) { go("/" + parts[...i].joined(separator: "/")) }
            }
        }
        .font(.callout.monospaced())
        .lineLimit(1)
    }
}

// MARK: - Images

struct ImagesView: View {
    @EnvironmentObject var engine: Engine
    @State private var selection = Set<DockerImage.ID>()
    @State private var showPull = false

    private var inUse: Set<String> { Set(engine.containers.map(\.image)) }

    var body: some View {
        Table(engine.images, selection: $selection) {
            TableColumn("Image") { image in
                VStack(alignment: .leading) {
                    Text(image.displayTags.first ?? "<untagged>").fontWeight(.medium)
                    if image.displayTags.count > 1 {
                        Text(image.displayTags.dropFirst().joined(separator: ", ")).font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            TableColumn("ID") { Text($0.shortID).monospaced().foregroundStyle(.secondary) }.width(110)
            TableColumn("Size") { Text(formatBytes($0.size)).monospacedDigit() }.width(90)
            TableColumn("Created") { image in
                // Reproducible builds (distroless, ko, nix) stamp 1970: no date.
                if image.created > 0 {
                    Text(Date(timeIntervalSince1970: TimeInterval(image.created)), style: .relative)
                } else {
                    Text("—").foregroundStyle(.secondary)
                }
            }
            .width(120)
            TableColumn("") { image in
                if image.displayTags.contains(where: inUse.contains) {
                    Text("in use").font(.caption).foregroundStyle(.secondary)
                }
            }
            .width(50)
        }
        .contextMenu(forSelectionType: DockerImage.ID.self) { ids in
            Button("Copy ID") { copy(ids.first ?? "") }
            Button("Delete", role: .destructive) { delete(ids) }
        }
        .toolbar {
            ToolbarItemGroup {
                Button { showPull = true } label: { Label("Pull", systemImage: "arrow.down.circle") }
                Button { delete(selection) } label: { Label("Delete", systemImage: "trash") }
                    .disabled(selection.isEmpty)
            }
        }
        .sheet(isPresented: $showPull) { PullSheet() }
        .navigationTitle("Images")
        .navigationSubtitle("\(engine.images.count) images · \(formatBytes(engine.images.reduce(0) { $0 + $1.size }))")
    }

    private func delete(_ ids: Set<DockerImage.ID>) {
        guard !ids.isEmpty, confirmDelete(ids.count == 1 ? "this image" : "\(ids.count) images", detail: "Containers using them must be removed first.") else { return }
        engine.perform("Delete image") { for id in ids { try await Docker.removeImage(id) } }
        selection.removeAll()
    }
}

struct PullSheet: View {
    @EnvironmentObject var engine: Engine
    @Environment(\.dismiss) private var dismiss
    @State private var reference = ""
    @State private var progress = PullProgress()
    @State private var pulling = false
    @State private var done = false
    @State private var error: String?
    /// The pull, cancelled with the sheet (its stream's connection closes,
    /// which stops Docker's pull).
    @State private var task: Task<Void, Never>?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Pull Image").font(.headline)
            TextField("e.g. postgres:16 or ghcr.io/owner/image", text: $reference)
                .textFieldStyle(.roundedBorder)
                .onSubmit(pull)
                .disabled(pulling)
            if pulling || done {
                if let fraction = progress.fraction {
                    ProgressView(value: fraction)
                } else {
                    ProgressView().progressViewStyle(.linear)
                }
                Text(done ? "Done" : progress.lastStatus).font(.caption).foregroundStyle(.secondary).lineLimit(2)
            }
            if let error { Text(error).font(.caption).foregroundStyle(.red) }
            HStack {
                Spacer()
                Button(done ? "Close" : "Cancel") {
                    task?.cancel()
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)
                Button("Pull", action: pull).keyboardShortcut(.defaultAction)
                    .disabled(reference.trimmingCharacters(in: .whitespaces).isEmpty || pulling)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private func pull() {
        let ref = reference.trimmingCharacters(in: .whitespaces)
        guard !ref.isEmpty, !pulling else { return }
        pulling = true; done = false; error = nil; progress = PullProgress()
        task = Task {
            do {
                for try await event in Docker.pull(ref) {
                    let status = event["status"] as? String ?? ""
                    if let id = event["id"] as? String {
                        let detail = event["progressDetail"] as? [String: Any]
                        let current = (detail?["current"] as? NSNumber)?.int64Value ?? 0
                        let total = (detail?["total"] as? NSNumber)?.int64Value ?? 0
                        let previous = progress.layers[id]
                        let complete = status.hasPrefix("Pull complete") || status.hasPrefix("Already exists")
                        let t = total > 0 ? total : previous?.total ?? 0
                        progress.layers[id] = (status, complete ? t : max(current, previous?.current ?? 0), t)
                        progress.lastStatus = "\(id): \(status)"
                    } else {
                        progress.lastStatus = status
                    }
                }
                done = true
                await engine.refreshAll()
            } catch {
                self.error = error.localizedDescription
            }
            pulling = false
        }
    }
}

// MARK: - Volumes

struct VolumesView: View {
    @EnvironmentObject var engine: Engine
    @State private var selection = Set<Volume.ID>()

    var body: some View {
        Table(engine.volumes, selection: $selection) {
            TableColumn("Name") { v in
                VStack(alignment: .leading) {
                    Text(v.name).fontWeight(.medium).lineLimit(1)
                    if let project = v.labels?["com.docker.compose.project"] {
                        Text(project).font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            TableColumn("Driver", value: \.driver).width(70)
            TableColumn("Created") { v in Text(v.createdAt.flatMap(parseDockerDate)?.formatted() ?? "") }.width(170)
        }
        .contextMenu(forSelectionType: Volume.ID.self) { ids in
            Button("Copy Name") { copy(ids.first ?? "") }
            if let name = ids.first, ids.count == 1 {
                Button("Open in Finder") {
                    NSWorkspace.shared.open(URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Berth/volumes/\(name)/_data"))
                }
            }
            Button("Delete", role: .destructive) { delete(ids) }
        }
        .toolbar {
            Button { delete(selection) } label: { Label("Delete", systemImage: "trash") }.disabled(selection.isEmpty)
        }
        .overlay { if engine.volumes.isEmpty { EmptyHint(icon: "externaldrive", text: "No volumes") } }
        .navigationTitle("Volumes")
    }

    private func delete(_ names: Set<Volume.ID>) {
        guard !names.isEmpty, confirmDelete(names.count == 1 ? "volume \(names.first ?? "")" : "\(names.count) volumes", detail: "Its data is deleted.") else { return }
        engine.perform("Delete volume") { for name in names { try await Docker.removeVolume(name) } }
        selection.removeAll()
    }
}

func parseDockerDate(_ s: String) -> Date? {
    let f = ISO8601DateFormatter()
    f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return f.date(from: s) ?? ISO8601DateFormatter().date(from: s)
}

// MARK: - Networks

struct NetworksView: View {
    @EnvironmentObject var engine: Engine
    @State private var selection = Set<DockerNetwork.ID>()

    var body: some View {
        Table(engine.networks, selection: $selection) {
            TableColumn("Name") { n in
                HStack {
                    Text(n.name).fontWeight(.medium)
                    if n.isBuiltin { Text("built-in").font(.caption).foregroundStyle(.secondary) }
                }
            }
            TableColumn("Driver", value: \.driver).width(80)
            TableColumn("Subnet") { n in Text(n.subnets.joined(separator: ", ")).monospaced() }
            TableColumn("Containers") { n in
                let count = engine.containers.filter { $0.networkSettings?.networks?[n.name] != nil }.count
                Text(count == 0 ? "" : "\(count)").monospacedDigit()
            }
            .width(80)
        }
        .contextMenu(forSelectionType: DockerNetwork.ID.self) { ids in
            let deletable = ids.filter { id in !(engine.networks.first { $0.id == id }?.isBuiltin ?? true) }
            Button("Delete", role: .destructive) { delete(deletable) }.disabled(deletable.isEmpty)
        }
        .navigationTitle("Networks")
        .navigationSubtitle(engine.status?.containerIps.map { "Container IPs: \($0)" } ?? "")
    }

    private func delete(_ ids: Set<DockerNetwork.ID>) {
        guard !ids.isEmpty, confirmDelete(ids.count == 1 ? "this network" : "\(ids.count) networks", detail: "Containers must be disconnected first.") else { return }
        engine.perform("Delete network") { for id in ids { try await Docker.removeNetwork(id) } }
        selection.removeAll()
    }
}
