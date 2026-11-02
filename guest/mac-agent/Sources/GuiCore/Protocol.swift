// The wire format of docs/gui-protocol.md: one JSON request line in, one JSON
// reply line out, optionally followed by `payload_len` bytes. One request per
// connection. Kept free of sockets so tests can drive it with plain Data.
import Foundation

public let protocolVersion = 1
public let agentVersion = "0.1.0"

/// The longest request line accepted. Requests are small JSON objects; a peer
/// that sends megabytes without a newline is broken or hostile.
public let maxRequestLine = 1 << 20

/// An error that goes back to the host as `{"ok":false,"error":message}`.
public struct GuiError: Error, Equatable, CustomStringConvertible {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

public struct Request {
    public var version: Int
    public var session: String?
    public var verb: String
    public var args: JSON
    /// Bytes that follow the request line (push's tar stream). Absent means
    /// "until the host closes its side".
    public var payloadLength: Int?

    public init(version: Int = protocolVersion, session: String? = nil, verb: String,
                args: JSON = [:], payloadLength: Int? = nil) {
        self.version = version
        self.session = session
        self.verb = verb
        self.args = args
        self.payloadLength = payloadLength
    }

    /// Parses one request line. A missing `v` is taken as version 1, the only
    /// one there is; any other version is rejected with the doc's wording.
    public static func parse(_ line: Data) throws -> Request {
        let value: JSON
        do {
            value = try JSON.parse(line)
        } catch {
            throw GuiError("bad request: not JSON")
        }
        guard value.object != nil else { throw GuiError("bad request: not a JSON object") }
        let v = value["v"]?.nonNull.flatMap { $0.int } ?? protocolVersion
        guard v == protocolVersion else { throw GuiError("unsupported version") }
        guard let verb = value["verb"]?.string, !verb.isEmpty else {
            throw GuiError("bad request: no verb")
        }
        let args = value["args"]?.nonNull ?? [:]
        guard args.object != nil else { throw GuiError("bad request: args must be an object") }
        var payload: Int?
        if let p = value["payload_len"]?.nonNull {
            guard let n = p.int, n >= 0 else { throw GuiError("bad request: payload_len") }
            payload = n
        }
        return Request(version: v, session: value["session"]?.string, verb: verb,
                       args: args, payloadLength: payload)
    }
}

public struct Reply {
    public var ok: Bool
    public var result: JSON
    public var error: String?
    public var payload: Data
    public var hit: JSON?

    public init(ok: Bool, result: JSON = [:], error: String? = nil, payload: Data = Data(), hit: JSON? = nil) {
        self.ok = ok
        self.result = result
        self.error = error
        self.payload = payload
        self.hit = hit
    }

    public static func success(_ result: JSON = [:], payload: Data = Data(), hit: JSON? = nil) -> Reply {
        Reply(ok: true, result: result, payload: payload, hit: hit)
    }

    public static func failure(_ message: String) -> Reply {
        Reply(ok: false, result: .null, error: message)
    }

    /// The reply line including its newline, then the payload.
    public func encoded() -> Data {
        var o: [String: JSON] = [
            "ok": .bool(ok),
            "result": result,
            "error": .from(error),
            "payload_len": .int(payload.count),
        ]
        if let hit { o["hit"] = hit }
        var out = JSON.object(o).encodedLine()
        out.append(0x0A)
        out.append(payload)
        return out
    }
}

/// Splits what a connection delivers into the request line and whatever
/// arrived after it. `read` returns the next chunk, or empty data at EOF.
public struct LineReader {
    public private(set) var buffered = Data()
    private let read: () throws -> Data

    public init(read: @escaping () throws -> Data) { self.read = read }

    /// Reads up to the first newline (not included). EOF before one ends the
    /// line too, so a client that forgets the newline but closes still works.
    public mutating func readLine(limit: Int = maxRequestLine) throws -> Data {
        while true {
            if let nl = buffered.firstIndex(of: 0x0A) {
                let line = buffered[buffered.startIndex..<nl]
                buffered = Data(buffered[(nl + 1)...])
                return Data(line)
            }
            if buffered.count > limit { throw GuiError("bad request: line too long") }
            let chunk = try read()
            if chunk.isEmpty {
                let line = buffered
                buffered = Data()
                return line
            }
            buffered.append(chunk)
        }
    }

    /// The bytes after the request line: exactly `length` of them, or all
    /// until EOF when `length` is nil. `sink` gets them chunk by chunk so a
    /// large push never sits in memory whole.
    public mutating func readPayload(length: Int?, sink: (Data) throws -> Void) throws {
        var remaining = length ?? Int.max
        if !buffered.isEmpty {
            let take = buffered.prefix(remaining)
            remaining -= take.count
            try sink(Data(take))
            buffered = Data()
        }
        while remaining > 0 {
            let chunk = try read()
            if chunk.isEmpty {
                if length != nil { throw GuiError("payload ended early: \(remaining) bytes missing") }
                return
            }
            let take = chunk.prefix(remaining)
            remaining -= take.count
            try sink(Data(take))
        }
    }
}
