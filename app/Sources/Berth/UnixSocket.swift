import Foundation

/// Blocking unix-domain socket helpers. Everything here runs off the main
/// thread; callers use the async wrappers in `UnixHTTP`.
final class UnixSocket {
    let fd: Int32

    /// With a `timeout`, a read or write that waits longer throws
    /// `SocketError.timedOut` instead of blocking forever. It bounds each
    /// call, not the whole exchange.
    init(path: String, timeout: TimeInterval? = nil) throws {
        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw SocketError.posix("socket", errno) }
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        if let timeout {
            var tv = timeval(tv_sec: Int(timeout), tv_usec: Int32((timeout - timeout.rounded(.down)) * 1_000_000))
            let len = socklen_t(MemoryLayout<timeval>.size)
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, len)
            setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, len)
        }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            close(fd)
            throw SocketError.message("socket path too long: \(path)")
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { buf in
            buf.copyBytes(from: bytes)
            buf[bytes.count] = 0
        }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0 else {
            let err = errno
            close(fd)
            throw SocketError.posix("connect \(path)", err)
        }
    }

    deinit { close(fd) }

    func write(_ data: Data) throws {
        try data.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < buf.count {
                let n = Darwin.write(fd, buf.baseAddress! + offset, buf.count - offset)
                if n < 0 {
                    if errno == EINTR { continue }
                    if errno == EAGAIN { throw SocketError.timedOut("write") }
                    throw SocketError.posix("write", errno)
                }
                offset += n
            }
        }
    }

    /// Returns nil at EOF.
    func read(max: Int = 65536) throws -> Data? {
        var buf = [UInt8](repeating: 0, count: max)
        while true {
            let n = Darwin.read(fd, &buf, max)
            if n < 0 {
                if errno == EINTR { continue }
                // SO_RCVTIMEO expired.
                if errno == EAGAIN { throw SocketError.timedOut("read") }
                throw SocketError.posix("read", errno)
            }
            return n == 0 ? nil : Data(buf[0..<n])
        }
    }

    /// Unblocks a reader on another thread; used to cancel streams.
    func shutdownBoth() { Darwin.shutdown(fd, SHUT_RDWR) }
    func shutdownWrite() { Darwin.shutdown(fd, SHUT_WR) }
}

enum SocketError: LocalizedError {
    case posix(String, Int32)
    case message(String)
    case timedOut(String)

    var errorDescription: String? {
        switch self {
        case let .posix(what, code): return "\(what): \(String(cString: strerror(code)))"
        case let .timedOut(what): return "Docker didn't respond (\(what) timed out)"
        case let .message(m): return m
        }
    }
}

struct HTTPResponse {
    var status: Int
    var headers: [String: String]
    var body: Data

    var text: String { String(decoding: body, as: UTF8.self) }
}

struct HTTPError: LocalizedError {
    var status: Int
    var message: String
    var errorDescription: String? { message.isEmpty ? "HTTP \(status)" : message }
}

/// HTTP/1.0 over a unix socket, so responses are never chunked: the body is
/// everything until the server closes. Streams likewise end at close.
enum UnixHTTP {
    /// One-shot request with a timeout picked for the endpoint, so a hung
    /// daemon can't hold the caller (and a thread) forever.
    static func request(
        _ socketPath: String, _ method: String, _ path: String, json: Any? = nil
    ) async throws -> HTTPResponse {
        try await request(socketPath, method, path, json: json, timeout: defaultTimeout(method, path))
    }

    /// `timeout` bounds each read/write; nil waits indefinitely.
    static func request(
        _ socketPath: String, _ method: String, _ path: String, json: Any? = nil, timeout: TimeInterval?
    ) async throws -> HTTPResponse {
        try await offPool {
            let sock = try UnixSocket(path: socketPath, timeout: timeout)
            try sock.write(requestData(method, path, json: json))
            var data = Data()
            while let chunk = try sock.read() { data.append(chunk) }
            let response = try parse(data)
            if response.status >= 400 {
                throw HTTPError(status: response.status, message: dockerMessage(response.body))
            }
            return response
        }
    }

    /// Docker answers these only once the work is done: a stop waits out the
    /// container's grace period, deleting an image removes its layers, and an
    /// exec start returns when the command exits.
    private static func defaultTimeout(_ method: String, _ path: String) -> TimeInterval {
        let route = path.split(separator: "?", maxSplits: 1).first.map(String.init) ?? path
        let slow: Bool
        switch method {
        case "POST":
            slow = route.hasSuffix("/stop") || route.hasSuffix("/restart")
                || (route.hasPrefix("/exec/") && route.hasSuffix("/start"))
        case "DELETE": slow = route.hasPrefix("/images/") || route.hasPrefix("/containers/")
        default: slow = false
        }
        return slow ? 300 : 30
    }

    /// Body chunks until the server closes or the consumer stops listening.
    static func stream(
        _ socketPath: String, _ method: String = "GET", _ path: String, json: Any? = nil
    ) -> AsyncThrowingStream<Data, Error> {
        AsyncThrowingStream { continuation in
            let box = SocketBox()
            continuation.onTermination = { _ in box.cancel() }
            Thread.detachNewThread {
                do {
                    let sock = try UnixSocket(path: socketPath)
                    // Cancelled while connecting: nothing would ever shut
                    // this socket down, and the read below would block.
                    guard box.set(sock) else {
                        continuation.finish()
                        return
                    }
                    try sock.write(requestData(method, path, json: json))
                    var head = Data()
                    var body: Data?
                    while body == nil, let chunk = try sock.read() {
                        head.append(chunk)
                        if let range = head.range(of: Data("\r\n\r\n".utf8)) {
                            let response = try parse(head[..<range.upperBound])
                            if response.status >= 400 {
                                var rest = Data(head[range.upperBound...])
                                while let more = try sock.read() { rest.append(more) }
                                throw HTTPError(status: response.status, message: dockerMessage(rest))
                            }
                            body = Data(head[range.upperBound...])
                        }
                    }
                    if let body, !body.isEmpty { continuation.yield(body) }
                    while let chunk = try sock.read() { continuation.yield(chunk) }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
        }
    }

    /// Send a request that upgrades to a raw bidirectional stream (Docker's
    /// hijacked exec/attach). Returns the socket and any bytes read past the
    /// response headers.
    static func upgrade(_ socketPath: String, _ path: String, json: Any) throws -> (UnixSocket, Data) {
        let sock = try UnixSocket(path: socketPath)
        let body = try JSONSerialization.data(withJSONObject: json)
        var head = "POST \(path) HTTP/1.1\r\nHost: docker\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n"
        head += "Content-Type: application/json\r\nContent-Length: \(body.count)\r\n\r\n"
        try sock.write(Data(head.utf8) + body)
        var data = Data()
        while let chunk = try sock.read() {
            data.append(chunk)
            if let range = data.range(of: Data("\r\n\r\n".utf8)) {
                let response = try parse(data[..<range.upperBound])
                guard response.status == 101 || response.status == 200 else {
                    throw HTTPError(status: response.status, message: "upgrade failed")
                }
                return (sock, Data(data[range.upperBound...]))
            }
        }
        throw SocketError.message("connection closed during upgrade")
    }

    private static func requestData(_ method: String, _ path: String, json: Any?) throws -> Data {
        var head = "\(method) \(path) HTTP/1.0\r\nHost: docker\r\n"
        var body = Data()
        if let json {
            body = try JSONSerialization.data(withJSONObject: json)
            head += "Content-Type: application/json\r\n"
        }
        head += "Content-Length: \(body.count)\r\n\r\n"
        return Data(head.utf8) + body
    }

    private static func parse(_ data: Data) throws -> HTTPResponse {
        guard let range = data.range(of: Data("\r\n\r\n".utf8)) else {
            throw SocketError.message("malformed HTTP response")
        }
        let head = String(decoding: data[..<range.lowerBound], as: UTF8.self)
        var lines = head.components(separatedBy: "\r\n")
        let statusLine = lines.removeFirst().split(separator: " ")
        guard statusLine.count >= 2, let status = Int(statusLine[1]) else {
            throw SocketError.message("malformed status line")
        }
        var headers: [String: String] = [:]
        for line in lines {
            if let colon = line.firstIndex(of: ":") {
                headers[line[..<colon].lowercased()] = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
            }
        }
        return HTTPResponse(status: status, headers: headers, body: Data(data[range.upperBound...]))
    }

    private static func dockerMessage(_ body: Data) -> String {
        if let obj = try? JSONSerialization.jsonObject(with: body) as? [String: Any],
           let message = obj["message"] as? String {
            return message
        }
        return String(decoding: body, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

/// The socket a stream reads from, set by its thread and shut down by its
/// cancellation, whichever comes first.
final class SocketBox: @unchecked Sendable {
    private let lock = NSLock()
    private var socket: UnixSocket?
    private var cancelled = false

    /// False (and the socket shut down) if the stream was already cancelled.
    func set(_ sock: UnixSocket) -> Bool {
        let ok = lock.withLock {
            if !cancelled { socket = sock }
            return !cancelled
        }
        if !ok { sock.shutdownBoth() }
        return ok
    }

    func cancel() {
        let sock = lock.withLock {
            cancelled = true
            return socket
        }
        sock?.shutdownBoth()
    }
}

/// Split Docker's multiplexed stdout/stderr stream (8-byte frame headers).
/// TTY containers send raw bytes; `isRaw` latches once that is detected.
struct StreamDemuxer {
    private var buffer = Data()
    private var isRaw = false

    mutating func feed(_ data: Data) -> [(stderr: Bool, data: Data)] {
        if isRaw { return [(false, data)] }
        buffer.append(data)
        var out: [(Bool, Data)] = []
        while buffer.count >= 8 {
            let b = [UInt8](buffer.prefix(8))
            guard b[0] <= 2, b[1] == 0, b[2] == 0, b[3] == 0 else {
                isRaw = true
                out.append((false, buffer))
                buffer.removeAll()
                return out
            }
            let size = Int(b[4]) << 24 | Int(b[5]) << 16 | Int(b[6]) << 8 | Int(b[7])
            guard buffer.count >= 8 + size else { break }
            let start = buffer.startIndex
            out.append((b[0] == 2, Data(buffer[(start + 8)..<(start + 8 + size)])))
            buffer.removeFirst(8 + size)
        }
        return out
    }
}
