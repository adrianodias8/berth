import SwiftUI

enum Section: String, CaseIterable, Identifiable {
    case containers = "Containers"
    case images = "Images"
    case volumes = "Volumes"
    case networks = "Networks"
    case machine = "Machine"
    var id: String { rawValue }

    var icon: String {
        switch self {
        case .containers: "shippingbox"
        case .images: "square.stack.3d.up"
        case .volumes: "externaldrive"
        case .networks: "network"
        case .machine: "terminal"
        }
    }
}

struct MainWindow: View {
    @EnvironmentObject var engine: Engine
    @AppStorage("onboarded") private var onboarded = false
    @State private var showOnboarding = false
    // `-openSection Images` etc. on the command line (NSArgumentDomain) picks
    // the starting screen; handy for scripted screenshots.
    @State private var section = Section(rawValue: UserDefaults.standard.string(forKey: "openSection") ?? "") ?? .containers

    var body: some View {
        NavigationSplitView {
            List(selection: $section) {
                SwiftUI.Section("Docker") {
                    ForEach([Section.containers, .images, .volumes, .networks]) { s in
                        Label(s.rawValue, systemImage: s.icon).tag(s)
                    }
                }
                SwiftUI.Section("Linux") {
                    Label(Section.machine.rawValue, systemImage: Section.machine.icon).tag(Section.machine)
                }
            }
            .navigationSplitViewColumnWidth(min: 170, ideal: 190)
            .safeAreaInset(edge: .bottom) { EngineBadge().padding(10) }
        } detail: {
            Group {
                if engine.isRunning || section == .machine {
                    switch section {
                    case .containers: ContainersView()
                    case .images: ImagesView()
                    case .volumes: VolumesView()
                    case .networks: NetworksView()
                    case .machine: MachineView()
                    }
                } else {
                    NotRunningView()
                }
            }
            .overlay(alignment: .bottom) { ErrorBanner() }
        }
        .sheet(isPresented: $showOnboarding) { OnboardingView() }
        .onAppear { if !onboarded { showOnboarding = true } }
        .onChange(of: onboarded) { if !onboarded { showOnboarding = true } }
    }
}

struct EngineBadge: View {
    @EnvironmentObject var engine: Engine

    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(engine.state.color).frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 1) {
                Text(engine.state.label).font(.callout)
                if let s = engine.status {
                    Text(s.memorySummary ?? "\(s.cpus) CPUs · \(s.memory >> 30) GiB").font(.caption2).foregroundStyle(.secondary)
                }
            }
            Spacer()
            Button {
                engine.isRunning ? engine.stop() : engine.start()
            } label: {
                Image(systemName: engine.isRunning ? "stop.fill" : "play.fill")
            }
            .buttonStyle(.borderless)
            .disabled(engine.state == .starting || engine.state == .stopping)
            .help(engine.isRunning ? "Stop Berth" : "Start Berth")
        }
        .padding(8)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
    }
}

struct NotRunningView: View {
    @EnvironmentObject var engine: Engine

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "ferry").font(.system(size: 48)).foregroundStyle(.secondary)
            switch engine.state {
            case .starting:
                ProgressView()
                Text("Starting Berth…").font(.title3)
                Text("The first start downloads Debian and installs Docker; it takes a few minutes.")
                    .foregroundStyle(.secondary)
            case .stopping:
                ProgressView()
                Text("Stopping…").font(.title3)
            case let .failed(message):
                Text("Berth failed to start").font(.title3)
                Text(message).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
                Button("Try Again") { engine.start() }
            default:
                Text("Berth is stopped").font(.title3)
                Button("Start") { engine.start() }.controlSize(.large)
            }
        }
        .multilineTextAlignment(.center)
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct ErrorBanner: View {
    @EnvironmentObject var engine: Engine

    var body: some View {
        if let error = engine.lastError {
            HStack {
                Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.yellow)
                Text(error).lineLimit(2).textSelection(.enabled)
                Spacer()
                Button { engine.lastError = nil } label: { Image(systemName: "xmark") }.buttonStyle(.borderless)
            }
            .padding(10)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
            .padding(12)
        }
    }
}

// MARK: - Containers

struct ContainersView: View {
    @EnvironmentObject var engine: Engine
    @State private var selection: Container.ID?
    @State private var appliedOpenContainer = false
    @State private var filter = ""

    private var groups: [(project: String?, containers: [Container])] {
        let visible = engine.containers.filter {
            filter.isEmpty || $0.name.localizedCaseInsensitiveContains(filter)
                || $0.image.localizedCaseInsensitiveContains(filter)
        }
        let projects = Dictionary(grouping: visible.filter { $0.composeProject != nil }, by: { $0.composeProject! })
        var result = projects.keys.sorted().map { (Optional($0), projects[$0]!) }
        let loose = visible.filter { $0.composeProject == nil }
        if !loose.isEmpty { result.append((nil, loose)) }
        return result
    }

    var body: some View {
        // A fixed-width list, not HSplitView: HSplitView sizes itself from its
        // children's ideal widths, and a long log line pushed it past the window.
        HStack(spacing: 0) {
            List(selection: $selection) {
                if engine.containers.isEmpty {
                    EmptyHint(icon: "shippingbox", text: "No containers yet.\nTry: docker run -d -p 8080:80 nginx")
                }
                ForEach(groups, id: \.project) { group in
                    if let project = group.project {
                        SwiftUI.Section {
                            ForEach(group.containers) { ContainerRow(container: $0).tag($0.id) }
                        } header: {
                            ProjectHeader(project: project, containers: group.containers)
                        }
                    } else {
                        SwiftUI.Section(groups.count > 1 ? "Containers" : "") {
                            ForEach(group.containers) { ContainerRow(container: $0).tag($0.id) }
                        }
                    }
                }
            }
            .searchable(text: $filter, placement: .toolbar, prompt: "Filter containers")
            .frame(width: 360)
            .onAppear(perform: applyOpenContainer)
            .onChange(of: engine.containers) { applyOpenContainer() }

            Divider()

            Group {
                if let id = selection, let container = engine.containers.first(where: { $0.id == id }) {
                    ContainerDetail(container: container).id(container.id)
                } else {
                    EmptyHint(icon: "sidebar.right", text: "Select a container")
                }
            }
            .frame(minWidth: 0, maxWidth: .infinity, maxHeight: .infinity)
        }
        .navigationTitle("Containers")
    }

    /// `-openContainer <name>` on the command line preselects a container.
    private func applyOpenContainer() {
        guard !appliedOpenContainer, let name = UserDefaults.standard.string(forKey: "openContainer"),
              let match = engine.containers.first(where: { $0.name == name }) else { return }
        selection = match.id
        appliedOpenContainer = true
    }
}

struct ProjectHeader: View {
    @EnvironmentObject var engine: Engine
    let project: String
    let containers: [Container]

    var body: some View {
        HStack {
            Image(systemName: "square.stack.3d.down.right")
            Text(project)
            Spacer()
            let anyRunning = containers.contains(where: \.isRunning)
            Button {
                let ids = containers.map(\.id)
                engine.perform(anyRunning ? "Stop project" : "Start project") {
                    for id in ids { try await anyRunning ? Docker.stop(id) : Docker.start(id) }
                }
            } label: {
                Image(systemName: anyRunning ? "stop.fill" : "play.fill")
            }
            .buttonStyle(.borderless)
            .help(anyRunning ? "Stop all in \(project)" : "Start all in \(project)")
        }
    }
}

struct ContainerRow: View {
    @EnvironmentObject var engine: Engine
    let container: Container

    var body: some View {
        HStack(spacing: 10) {
            Circle().fill(container.isRunning ? .green : .gray.opacity(0.6)).frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 2) {
                Text(container.composeService ?? container.name).fontWeight(.medium).lineLimit(1)
                Text(container.image).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer()
            ForEach(container.publishedPorts.prefix(2), id: \.publicPort) { port in
                Text(verbatim: ":\(port.publicPort!)").font(.caption.monospaced()).foregroundStyle(.secondary)
            }
            ContainerActions(container: container)
        }
        .padding(.vertical, 2)
        .contextMenu { ContainerMenu(container: container) }
    }
}

struct ContainerActions: View {
    @EnvironmentObject var engine: Engine
    let container: Container

    var body: some View {
        HStack(spacing: 6) {
            if container.isRunning {
                if let url = webURL(for: container) {
                    Button { NSWorkspace.shared.open(url) } label: { Image(systemName: "safari") }
                        .help("Open \(url.absoluteString)")
                }
                Button { engine.perform("Stop") { try await Docker.stop(container.id) } } label: {
                    Image(systemName: "stop.fill")
                }
                .help("Stop")
            } else {
                Button { engine.perform("Start") { try await Docker.start(container.id) } } label: {
                    Image(systemName: "play.fill")
                }
                .help("Start")
            }
        }
        .buttonStyle(.borderless)
    }
}

struct ContainerMenu: View {
    @EnvironmentObject var engine: Engine
    let container: Container

    var body: some View {
        if container.isRunning {
            Button("Stop") { engine.perform("Stop") { try await Docker.stop(container.id) } }
            Button("Restart") { engine.perform("Restart") { try await Docker.restart(container.id) } }
        } else {
            Button("Start") { engine.perform("Start") { try await Docker.start(container.id) } }
        }
        Divider()
        if let url = webURL(for: container) {
            Button("Open \(url.host ?? "") in Browser") { NSWorkspace.shared.open(url) }
        }
        Button("Copy Domain") { copy(container.domain) }
        if let ip = container.ipAddresses.first { Button("Copy IP (\(ip))") { copy(ip) } }
        Button("Copy ID") { copy(container.id) }
        Divider()
        Button("Delete", role: .destructive) { engine.perform("Delete") { try await Docker.remove(container.id) } }
    }
}

func copy(_ text: String) {
    NSPasteboard.general.clearContents()
    NSPasteboard.general.setString(text, forType: .string)
}

struct EmptyHint: View {
    let icon: String
    let text: String

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: icon).font(.system(size: 32)).foregroundStyle(.tertiary)
            Text(text).multilineTextAlignment(.center).foregroundStyle(.secondary).textSelection(.enabled)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding()
    }
}
