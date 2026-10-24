import Foundation

// MARK: - Models (subset of the Docker Engine API)

struct Container: Decodable, Identifiable, Hashable {
    let id: String
    let names: [String]
    let image: String
    let state: String
    let status: String
    let created: Int
    let labels: [String: String]?
    let ports: [Port]?
    let networkSettings: NetworkSettings?

    enum CodingKeys: String, CodingKey {
        case id = "Id", names = "Names", image = "Image", state = "State", status = "Status"
        case created = "Created", labels = "Labels", ports = "Ports", networkSettings = "NetworkSettings"
    }

    struct Port: Decodable, Hashable {
        let ip: String?
        let privatePort: Int
        let publicPort: Int?
        let type: String
        enum CodingKeys: String, CodingKey { case ip = "IP", privatePort = "PrivatePort", publicPort = "PublicPort", type = "Type" }
    }

    struct NetworkSettings: Decodable, Hashable {
        let networks: [String: Endpoint]?
        enum CodingKeys: String, CodingKey { case networks = "Networks" }
    }

    struct Endpoint: Decodable, Hashable {
        let ipAddress: String?
        enum CodingKeys: String, CodingKey { case ipAddress = "IPAddress" }
    }

    var name: String { names.first.map { String($0.dropFirst()) } ?? String(id.prefix(12)) }
    var isRunning: Bool { state == "running" }
    var composeProject: String? { labels?["com.docker.compose.project"] }
    var composeService: String? { labels?["com.docker.compose.service"] }
    var ipAddresses: [String] {
        (networkSettings?.networks ?? [:]).values.compactMap(\.ipAddress).filter { !$0.isEmpty }
    }

    /// Distinct published TCP ports, one per host port.
    var publishedPorts: [Port] {
        var seen = Set<Int>()
        return (ports ?? []).filter { p in
            guard let host = p.publicPort, p.type == "tcp" else { return false }
            return seen.insert(host).inserted
        }.sorted { ($0.publicPort ?? 0) < ($1.publicPort ?? 0) }
    }

    /// The name berth's guest agent answers for this container.
    var domain: String { "\(name.lowercased()).berth.local" }
}

struct DockerImage: Decodable, Identifiable, Hashable {
    let id: String
    let repoTags: [String]?
    let size: Int64
    let created: Int
    let containers: Int?

    enum CodingKeys: String, CodingKey {
        case id = "Id", repoTags = "RepoTags", size = "Size", created = "Created", containers = "Containers"
    }

    var displayTags: [String] { (repoTags ?? []).filter { $0 != "<none>:<none>" } }
    var shortID: String { String(id.replacingOccurrences(of: "sha256:", with: "").prefix(12)) }
}

struct Volume: Decodable, Identifiable, Hashable {
    let name: String
    let driver: String
    let mountpoint: String
    let createdAt: String?
    let labels: [String: String]?
    var id: String { name }

    enum CodingKeys: String, CodingKey {
        case name = "Name", driver = "Driver", mountpoint = "Mountpoint", createdAt = "CreatedAt", labels = "Labels"
    }
}

struct DockerNetwork: Decodable, Identifiable, Hashable {
    let id: String
    let name: String
    let driver: String
    let scope: String
    let ipam: IPAM?

    enum CodingKeys: String, CodingKey { case id = "Id", name = "Name", driver = "Driver", scope = "Scope", ipam = "IPAM" }

    struct IPAM: Decodable, Hashable {
        let config: [Config]?
        enum CodingKeys: String, CodingKey { case config = "Config" }
    }

    struct Config: Decodable, Hashable {
        let subnet: String?
        let gateway: String?
        enum CodingKeys: String, CodingKey { case subnet = "Subnet", gateway = "Gateway" }
    }

    var subnets: [String] { (ipam?.config ?? []).compactMap(\.subnet) }
    var isBuiltin: Bool { ["bridge", "host", "none"].contains(name) }
}

struct StatsSample: Identifiable {
    let id = UUID()
    let time: Date
    let cpuPercent: Double
    let memoryBytes: Double
    let memoryLimit: Double
}

struct FileEntry: Identifiable, Hashable {
    let name: String
    let kind: String
    let size: Int64
    let modified: Date
    var id: String { name }
    var isDirectory: Bool { kind == "directory" }
    var isLink: Bool { kind == "symbolic link" }
}

struct PullProgress {
    var layers: [String: (status: String, current: Int64, total: Int64)] = [:]
    var lastStatus = ""

    var fraction: Double? {
        let totals = layers.values.filter { $0.total > 0 }
        guard !totals.isEmpty else { return nil }
        let done = totals.reduce(0) { $0 + min($1.current, $1.total) }
        return Double(done) / Double(totals.reduce(0) { $0 + $1.total })
    }
}

// MARK: - Client

/// Docker Engine API over berth's socket.
enum Docker {
    static var socket: String { BerthPaths.dockerSocket }

    static func containers() async throws -> [Container] {
        try decode(try await UnixHTTP.request(socket, "GET", "/containers/json?all=1"))
    }

    static func images() async throws -> [DockerImage] {
        try decode(try await UnixHTTP.request(socket, "GET", "/images/json"))
    }

    static func volumes() async throws -> [Volume] {
        struct List: Decodable {
            let volumes: [Volume]?
            enum CodingKeys: String, CodingKey { case volumes = "Volumes" }
        }
        let list: List = try decode(try await UnixHTTP.request(socket, "GET", "/volumes"))
        return list.volumes ?? []
    }

    static func networks() async throws -> [DockerNetwork] {
        try decode(try await UnixHTTP.request(socket, "GET", "/networks"))
    }

    static func start(_ id: String) async throws { _ = try await UnixHTTP.request(socket, "POST", "/containers/\(id)/start") }
    static func stop(_ id: String) async throws { _ = try await UnixHTTP.request(socket, "POST", "/containers/\(id)/stop") }
    static func restart(_ id: String) async throws { _ = try await UnixHTTP.request(socket, "POST", "/containers/\(id)/restart") }
    static func remove(_ id: String) async throws {
        _ = try await UnixHTTP.request(socket, "DELETE", "/containers/\(id)?force=1&v=1")
    }
    static func removeImage(_ id: String) async throws { _ = try await UnixHTTP.request(socket, "DELETE", "/images/\(id)") }
    static func removeVolume(_ name: String) async throws {
        _ = try await UnixHTTP.request(socket, "DELETE", "/volumes/\(escape(name))")
    }
    static func removeNetwork(_ id: String) async throws { _ = try await UnixHTTP.request(socket, "DELETE", "/networks/\(id)") }

    static func inspect(_ id: String) async throws -> [String: Any] {
        let response = try await UnixHTTP.request(socket, "GET", "/containers/\(id)/json")
        return (try JSONSerialization.jsonObject(with: response.body) as? [String: Any]) ?? [:]
    }

    /// Follows logs; yields text with stderr lines marked.
    static func logs(_ id: String, tail: Int = 500) -> AsyncThrowingStream<String, Error> {
        let raw = UnixHTTP.stream(socket, "GET", "/containers/\(id)/logs?stdout=1&stderr=1&follow=1&timestamps=0&tail=\(tail)")
        return AsyncThrowingStream { continuation in
            let task = Task {
                var demux = StreamDemuxer()
                do {
                    for try await chunk in raw {
                        for frame in demux.feed(chunk) {
                            continuation.yield(String(decoding: frame.data, as: UTF8.self))
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// One CPU/memory sample (the daemon waits for two readings internally).
    static func stats(_ id: String) async throws -> StatsSample {
        let response = try await UnixHTTP.request(socket, "GET", "/containers/\(id)/stats?stream=false")
        let obj = (try JSONSerialization.jsonObject(with: response.body) as? [String: Any]) ?? [:]
        func num(_ path: [String]) -> Double {
            var node: Any? = obj
            for key in path { node = (node as? [String: Any])?[key] }
            return (node as? NSNumber)?.doubleValue ?? 0
        }
        let cpuDelta = num(["cpu_stats", "cpu_usage", "total_usage"]) - num(["precpu_stats", "cpu_usage", "total_usage"])
        let sysDelta = num(["cpu_stats", "system_cpu_usage"]) - num(["precpu_stats", "system_cpu_usage"])
        let cpus = max(num(["cpu_stats", "online_cpus"]), 1)
        let cpu = sysDelta > 0 && cpuDelta > 0 ? cpuDelta / sysDelta * cpus * 100 : 0
        // Match `docker stats`: usage minus reclaimable page cache (cgroup v2).
        let memory = max(num(["memory_stats", "usage"]) - num(["memory_stats", "stats", "inactive_file"]), 0)
        return StatsSample(time: Date(), cpuPercent: cpu, memoryBytes: memory, memoryLimit: num(["memory_stats", "limit"]))
    }

    /// Run a command in a container and collect stdout (and stderr).
    static func exec(_ id: String, _ cmd: [String]) async throws -> (stdout: Data, stderr: String, exitCode: Int) {
        let created = try await UnixHTTP.request(
            socket, "POST", "/containers/\(id)/exec",
            json: ["Cmd": cmd, "AttachStdout": true, "AttachStderr": true]
        )
        guard let execID = (try JSONSerialization.jsonObject(with: created.body) as? [String: Any])?["Id"] as? String else {
            throw SocketError.message("exec create returned no id")
        }
        let started = try await UnixHTTP.request(socket, "POST", "/exec/\(execID)/start", json: ["Detach": false, "Tty": false])
        var demux = StreamDemuxer()
        var out = Data(), err = Data()
        for frame in demux.feed(started.body) {
            if frame.stderr { err.append(frame.data) } else { out.append(frame.data) }
        }
        let inspect = try await UnixHTTP.request(socket, "GET", "/exec/\(execID)/json")
        let code = ((try JSONSerialization.jsonObject(with: inspect.body) as? [String: Any])?["ExitCode"] as? Int) ?? -1
        return (out, String(decoding: err, as: UTF8.self), code)
    }

    /// Directory listing via `stat`, which both busybox and coreutils provide.
    static func listFiles(_ id: String, path: String) async throws -> [FileEntry] {
        let script = #"cd -- "$1" || exit 1; for f in * .*; do case "$f" in .|..) continue;; esac; [ -e "$f" ] || [ -L "$f" ] || continue; stat -c '%F|%s|%Y|%n' -- "$f"; done"#
        let result = try await exec(id, ["sh", "-c", script, "sh", path])
        if result.exitCode != 0 {
            throw SocketError.message(result.stderr.isEmpty ? "cannot list \(path) (does the image have a shell?)" : result.stderr)
        }
        return String(decoding: result.stdout, as: UTF8.self).split(separator: "\n").compactMap { line in
            let parts = line.split(separator: "|", maxSplits: 3, omittingEmptySubsequences: false)
            guard parts.count == 4 else { return nil }
            return FileEntry(
                name: String(parts[3]), kind: String(parts[0]), size: Int64(parts[1]) ?? 0,
                modified: Date(timeIntervalSince1970: TimeInterval(parts[2]) ?? 0)
            )
        }.sorted { ($0.isDirectory ? 0 : 1, $0.name.lowercased()) < ($1.isDirectory ? 0 : 1, $1.name.lowercased()) }
    }

    /// File contents via the archive endpoint (a tar with one entry).
    static func readFile(_ id: String, path: String) async throws -> Data {
        let response = try await UnixHTTP.request(socket, "GET", "/containers/\(id)/archive?path=\(escape(path))")
        return try firstTarEntry(response.body)
    }

    /// Streams pull progress; ends when the pull completes.
    static func pull(_ reference: String) -> AsyncThrowingStream<[String: Any], Error> {
        var name = reference, tag = "latest"
        if let at = reference.firstIndex(of: "@") {
            name = String(reference[..<at]); tag = String(reference[reference.index(after: at)...])
        } else if let colon = reference.lastIndex(of: ":"), !reference[colon...].contains("/") {
            name = String(reference[..<colon]); tag = String(reference[reference.index(after: colon)...])
        }
        let raw = UnixHTTP.stream(socket, "POST", "/images/create?fromImage=\(escape(name))&tag=\(escape(tag))")
        return AsyncThrowingStream { continuation in
            let task = Task {
                var buffer = Data()
                do {
                    for try await chunk in raw {
                        buffer.append(chunk)
                        while let nl = buffer.firstIndex(of: 0x0a) {
                            let line = buffer[buffer.startIndex..<nl]
                            buffer.removeSubrange(buffer.startIndex...nl)
                            guard let obj = try? JSONSerialization.jsonObject(with: line) as? [String: Any] else { continue }
                            if let error = obj["error"] as? String { throw SocketError.message(error) }
                            continuation.yield(obj)
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// Events that change the lists (not healthchecks, execs or attaches,
    /// which a busy stack sends constantly).
    static func events() -> AsyncThrowingStream<Data, Error> {
        let filters = #"{"event":["create","start","die","destroy","pause","unpause","rename","update","pull","delete","tag","untag","import","load","connect","disconnect","prune"]}"#
        return UnixHTTP.stream(socket, "GET", "/events?filters=" + (filters.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? ""))
    }

    static func decode<T: Decodable>(_ response: HTTPResponse) throws -> T {
        try JSONDecoder().decode(T.self, from: response.body)
    }

    static func escape(_ s: String) -> String {
        s.addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed.subtracting(CharacterSet(charactersIn: "&=+?#"))) ?? s
    }

    /// The archive's file: skips PAX (`x`, `g`) and GNU long-name (`L`, `K`)
    /// entries before it, which Docker adds for long names and xattrs; a PAX
    /// `size` overrides the header's.
    static func firstTarEntry(_ tar: Data) throws -> Data {
        let bytes = [UInt8](tar)
        var offset = 0
        var paxSize: Int?
        while offset + 512 <= bytes.count {
            let header = bytes[offset..<offset + 512]
            if header.allSatisfy({ $0 == 0 }) { break }
            guard let headerSize = tarNumber(header.dropFirst(124).prefix(12)) else { throw SocketError.message("bad tar header") }
            let type = header[header.startIndex + 156]
            let dataStart = offset + 512
            let size = (type == 0 || type == 0x30) ? (paxSize ?? headerSize) : headerSize
            guard size >= 0, dataStart + size <= bytes.count else { throw SocketError.message("truncated archive") }
            let data = bytes[dataStart..<dataStart + size]
            switch type {
            case 0, 0x30:
                return Data(data)
            case UInt8(ascii: "x"):
                paxSize = paxRecords(data)["size"].flatMap { Int($0) }
            case UInt8(ascii: "g"), UInt8(ascii: "L"), UInt8(ascii: "K"):
                break
            default:
                throw SocketError.message("not a regular file")
            }
            offset = dataStart + (size + 511) / 512 * 512
        }
        throw SocketError.message("empty archive")
    }

    /// An octal tar number field, or base-256 (high bit set) for big values.
    private static func tarNumber(_ field: ArraySlice<UInt8>) -> Int? {
        guard let first = field.first else { return nil }
        if first & 0x80 != 0 {
            return field.dropFirst().reduce(Int(first & 0x7f)) { $0 << 8 | Int($1) }
        }
        let text = String(decoding: field.prefix { $0 != 0 }, as: UTF8.self).trimmingCharacters(in: .whitespaces)
        return text.isEmpty ? 0 : Int(text, radix: 8)
    }

    /// PAX records: "<length> <key>=<value>\n" each.
    private static func paxRecords(_ data: ArraySlice<UInt8>) -> [String: String] {
        var records: [String: String] = [:]
        for line in String(decoding: data, as: UTF8.self).split(separator: "\n") {
            guard let space = line.firstIndex(of: " ") else { continue }
            let record = line[line.index(after: space)...]
            guard let eq = record.firstIndex(of: "=") else { continue }
            records[String(record[..<eq])] = String(record[record.index(after: eq)...])
        }
        return records
    }
}
