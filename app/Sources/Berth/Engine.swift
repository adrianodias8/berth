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
    /// Why file sharing is slower than it could be, if it is.
    let fileSharingNote: String?

    enum CodingKeys: String, CodingKey {
        case pid, vm, docker, cpus, memory, ports
        case containerIps = "container_ips"
        case memoryUsed = "memory_used"
        case memoryAllowed = "memory_allowed"
        case fileSharingNote = "file_sharing_note"
    }

    /// The root helper predates abilities this berth needs; reinstall it.
    var helperOutdated: Bool { containerIps?.contains("outdated") ?? false }

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
    /// "berth" (its own share, macOS 27; the default) or "apple" (src/virtiofs.rs).
    var fileSharing: String?
    /// IPv6 for containers (agent/src/ipv6.rs).
    var ipv6: Bool?
    /// "shutdown" (default, like OrbStack) or "suspend": what `berth stop` does.
    var stop: String?
    /// Docker volumes and machines in Finder at ~/Berth (src/files.rs); default on.
    var finder: Bool?
    /// "auto" (the Mac's proxy settings; the default), "none" or a URL (src/proxy.rs).
    var proxy: String?
    /// "localhost": published ports only on the Mac's loopback (src/ports.rs).
    var ports: String?

    enum CodingKeys: String, CodingKey, CaseIterable {
        case cpus
        case memoryMib = "memory_mib"
        case fileSharing = "file_sharing"
        case ipv6
        case stop
        case finder
        case proxy
        case ports
    }

    static func load() -> BerthConfig {
        (try? JSONDecoder().decode(BerthConfig.self, from: Data(contentsOf: BerthPaths.config))) ?? BerthConfig()
    }

    /// Write the keys that differ from `baseline` into config.json as it is
    /// now: keys the app doesn't know, and changes made meanwhile with
    /// `berth config`, stay. A file that isn't valid JSON is left alone.
    func save(over baseline: BerthConfig) throws {
        try FileManager.default.createDirectory(at: BerthPaths.root, withIntermediateDirectories: true)
        var file: [String: Any] = [:]
        if let data = try? Data(contentsOf: BerthPaths.config), !data.isEmpty {
            guard let dict = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
                throw NSError(domain: "berth", code: 1, userInfo: [NSLocalizedDescriptionKey:
                    "~/.berth/config.json isn't valid JSON; fix it (or `berth config list`) first"])
            }
            file = dict
        }
        let mine = try Self.dictionary(self), theirs = try Self.dictionary(baseline)
        for key in CodingKeys.allCases.map(\.rawValue) {
            let new = mine[key] as? NSObject, old = theirs[key] as? NSObject
            guard new != old else { continue }
            file[key] = new
        }
        let data = try JSONSerialization.data(withJSONObject: file, options: [.prettyPrinted, .sortedKeys])
        try data.write(to: BerthPaths.config, options: .atomic)
    }

    private static func dictionary(_ config: BerthConfig) throws -> [String: Any] {
        (try JSONSerialization.jsonObject(with: JSONEncoder().encode(config)) as? [String: Any]) ?? [:]
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
    /// The snapshot being created, restored or deleted, kept here rather
    /// than in the view so leaving the screen mid-operation doesn't allow a
    /// second one; and the last one's error.
    @Published var snapshotBusy: String?
    @Published var snapshotError: String?

    private var pollTask: Task<Void, Never>?
    private var eventsTask: Task<Void, Never>?
    private var refreshPending = false
    private var polls = 0

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

    /// `berth start` hasn't exited yet: until then no status just means booting.
    private var startInFlight = false
    private var stopInFlight = false
    private var stopFailed = false

    func start() {
        guard state != .starting, state != .running else { return }
        state = .starting
        lastError = nil
        startInFlight = true
        Task.detached {
            let result = Self.run([BerthPaths.cli, "start"])
            await MainActor.run {
                self.startInFlight = false
                if result.code != 0 {
                    self.state = .failed(result.output.trimmingCharacters(in: .whitespacesAndNewlines))
                    self.lastError = result.output
                }
            }
        }
    }

    /// `shutdown` overrides the "stop" setting (a restart must apply settings,
    /// which a suspended VM wouldn't).
    func stop(shutdown: Bool = false) {
        guard state == .running || state == .starting else { return }
        state = .stopping
        stopInFlight = true
        Task.detached {
            let result = Self.run([BerthPaths.cli, "stop"] + (shutdown ? ["--shutdown"] : []))
            await MainActor.run { self.stopInFlight = false }
            guard result.code != 0 else { return }
            await MainActor.run {
                // Still up: leave "Stopping…" so the buttons work again.
                self.lastError = result.output
                self.stopFailed = true
                self.state = self.status?.docker == true ? .running : .failed(result.output.trimmingCharacters(in: .whitespacesAndNewlines))
            }
        }
    }

    /// `berth restart`: it shuts down (so settings apply) and starts, and
    /// knows when each is done.
    func restart() {
        guard state == .running || state == .starting else { return }
        state = .starting
        lastError = nil
        startInFlight = true
        Task.detached {
            let result = Self.run([BerthPaths.cli, "restart"])
            await MainActor.run {
                self.startInFlight = false
                if result.code != 0 {
                    self.state = .failed(result.output.trimmingCharacters(in: .whitespacesAndNewlines))
                    self.lastError = result.output
                }
            }
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
        case (nil, .starting) where !startInFlight:
            // `berth start` returned but the daemon is gone.
            state = .stopped
        case (nil, .failed) where stopFailed:
            // A stop that timed out (a long suspend) finished after all; a
            // failed start stays failed, so its message stays up.
            state = .stopped
            stopFailed = false
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
        // Container domains come with Docker events (refreshAll); machines'
        // change without one, so every 30 s too. Each request wakes the VM.
        polls += 1
        if state == .running, domains.isEmpty || polls % 15 == 0 {
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
