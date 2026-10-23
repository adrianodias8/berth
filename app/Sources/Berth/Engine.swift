import Foundation
import SwiftUI

enum BerthPaths {
    static let root = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".berth")
    static var controlSocket: String { root.appendingPathComponent("run/berth.sock").path }
    static var dockerSocket: String { root.appendingPathComponent("run/docker.sock").path }
    static var config: URL { root.appendingPathComponent("config.json") }
    static var daemonLog: URL { root.appendingPathComponent("log/daemon.log") }

    /// The `berth` CLI: bundled in Berth.app, else next to a dev build, else PATH.
    static var cli: String {
        let bundled = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/berth").path
        if FileManager.default.isExecutableFile(atPath: bundled) { return bundled }
        for candidate in ["/usr/local/bin/berth", "/opt/homebrew/bin/berth"]
        where FileManager.default.isExecutableFile(atPath: candidate) {
            return candidate
        }
        return "berth"
    }
}

/// `berth status` as reported by the daemon's control socket.
struct BerthStatus: Decodable, Equatable {
    let pid: Int
    let vm: String
    let docker: Bool
    let cpus: Int
    let memory: UInt64
    let ports: [String]?
    let containerIps: String?
    /// Guest memory in use and what the VM currently holds; the rest of
    /// `memory` has been returned to macOS.
    let memoryUsed: UInt64?
    let memoryAllowed: UInt64?

    enum CodingKeys: String, CodingKey {
        case pid, vm, docker, cpus, memory, ports
        case containerIps = "container_ips"
        case memoryUsed = "memory_used"
        case memoryAllowed = "memory_allowed"
    }

    /// e.g. "1.2 GiB used" for compact displays.
    var memorySummary: String? {
        guard let used = memoryUsed, used > 0 else { return nil }
        return String(format: "%.1f GiB used", Double(used) / 1_073_741_824)
    }
}

struct BerthDomain: Decodable, Hashable, Identifiable {
    let domain: String
    let wildcard: Bool
    let subdomainsOnly: Bool
    let ip: String
    let container: String
    var id: String { domain }

    enum CodingKeys: String, CodingKey {
        case domain, wildcard, ip, container
        case subdomainsOnly = "subdomains_only"
    }
}

enum Control {
    /// One JSON request line, one JSON reply line.
    static func request<T: Decodable>(_ cmd: String) async throws -> T {
        try await Task.detached {
            let sock = try UnixSocket(path: BerthPaths.controlSocket)
            var tv = timeval(tv_sec: 10, tv_usec: 0)
            setsockopt(sock.fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
            try sock.write(Data("{\"cmd\":\"\(cmd)\"}\n".utf8))
            var data = Data()
            while !data.contains(0x0a), let chunk = try sock.read() { data.append(chunk) }
            return try JSONDecoder().decode(T.self, from: data)
        }.value
    }
}

struct BerthConfig: Codable, Equatable {
    var cpus: Int?
    var memoryMib: Int?

    enum CodingKeys: String, CodingKey {
        case cpus
        case memoryMib = "memory_mib"
    }

    static func load() -> BerthConfig {
        (try? JSONDecoder().decode(BerthConfig.self, from: Data(contentsOf: BerthPaths.config))) ?? BerthConfig()
    }

    func save() throws {
        try FileManager.default.createDirectory(at: BerthPaths.root, withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        try encoder.encode(self).write(to: BerthPaths.config, options: .atomic)
    }
}

enum EngineState: Equatable {
    case stopped, starting, running, stopping
    case failed(String)

    var label: String {
        switch self {
        case .stopped: "Stopped"
        case .starting: "Starting…"
        case .running: "Running"
        case .stopping: "Stopping…"
        case .failed: "Error"
        }
    }

    var color: Color {
        switch self {
        case .running: .green
        case .starting, .stopping: .orange
        case .stopped: .secondary
        case .failed: .red
        }
    }
}

/// App-wide state: the VM's status plus live Docker lists.
@MainActor
final class Engine: ObservableObject {
    @Published var state: EngineState = .stopped
    @Published var status: BerthStatus?
    @Published var containers: [Container] = []
    @Published var images: [DockerImage] = []
    @Published var volumes: [Volume] = []
    @Published var networks: [DockerNetwork] = []
    @Published var domains: [BerthDomain] = []
    @Published var lastError: String?

    private var pollTask: Task<Void, Never>?
    private var eventsTask: Task<Void, Never>?
    private var refreshPending = false

    init() {
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                await self?.poll()
                try? await Task.sleep(for: .seconds(2))
            }
        }
        if UserDefaults.standard.object(forKey: "autoStart") as? Bool ?? true {
            Task { [weak self] in
                try? await Task.sleep(for: .milliseconds(300))
                if self?.status == nil { self?.start() }
            }
        }
    }

    var isRunning: Bool { state == .running }

    // MARK: VM lifecycle

    func start() {
        guard state != .starting, state != .running else { return }
        state = .starting
        lastError = nil
        Task.detached {
            let result = Self.run([BerthPaths.cli, "start"])
            await MainActor.run {
                if result.code != 0 {
                    self.state = .failed(result.output.trimmingCharacters(in: .whitespacesAndNewlines))
                    self.lastError = result.output
                }
            }
        }
    }

    func stop() {
        guard state == .running || state == .starting else { return }
        state = .stopping
        Task.detached {
            _ = Self.run([BerthPaths.cli, "stop"])
        }
    }

    func restart() {
        Task {
            stop()
            while status != nil { try? await Task.sleep(for: .milliseconds(500)) }
            start()
        }
    }

    nonisolated static func run(_ argv: [String]) -> (code: Int32, output: String) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: argv[0])
        process.arguments = Array(argv.dropFirst())
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        do {
            try process.run()
        } catch {
            return (-1, "could not run \(argv[0]): \(error.localizedDescription)")
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return (process.terminationStatus, String(decoding: data, as: UTF8.self))
    }

    private func poll() async {
        let new: BerthStatus? = try? await Control.request("status")
        if new != status { status = new }
        switch (new, state) {
        case (nil, .stopping), (nil, .running):
            state = .stopped
            clearLists()
        case (let s?, _) where s.docker:
            if state != .running && state != .stopping {
                state = .running
                startEvents()
                await refreshAll()
            }
        case (_?, .stopped):
            state = .starting
        default:
            break
        }
        if state == .running, domains.isEmpty || Int.random(in: 0..<5) == 0 {
            domains = (try? await Control.request("domains")) ?? domains
        }
    }

    private func clearLists() {
        eventsTask?.cancel()
        eventsTask = nil
        containers = []; images = []; volumes = []; networks = []; domains = []
    }

    // MARK: Docker lists

    private func startEvents() {
        eventsTask?.cancel()
        eventsTask = Task { [weak self] in
            while !Task.isCancelled {
                do {
                    for try await _ in Docker.events() { self?.scheduleRefresh() }
                } catch {}
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    /// Coalesce bursts of events (compose up emits dozens) into one refresh.
    private func scheduleRefresh() {
        guard !refreshPending else { return }
        refreshPending = true
        Task {
            try? await Task.sleep(for: .milliseconds(250))
            refreshPending = false
            await refreshAll()
        }
    }

    func refreshAll() async {
        guard state == .running else { return }
        async let c = Docker.containers()
        async let i = Docker.images()
        async let v = Docker.volumes()
        async let n = Docker.networks()
        async let d: [BerthDomain] = Control.request("domains")
        if let c = try? await c { containers = c.sorted { $0.name < $1.name } }
        if let i = try? await i { images = i.sorted { ($0.displayTags.first ?? "~") < ($1.displayTags.first ?? "~") } }
        if let v = try? await v { volumes = v.sorted { $0.name < $1.name } }
        if let n = try? await n { networks = n.sorted { $0.name < $1.name } }
        if let d = try? await d { domains = d }
    }

    /// Run a Docker action, surfacing failures in the UI.
    func perform(_ what: String, _ action: @escaping () async throws -> Void) {
        Task {
            do {
                try await action()
                await refreshAll()
            } catch {
                lastError = "\(what): \(error.localizedDescription)"
            }
        }
    }

    func domains(for container: Container) -> [BerthDomain] {
        domains.filter { $0.container == container.name && !$0.subdomainsOnly }
    }
}
