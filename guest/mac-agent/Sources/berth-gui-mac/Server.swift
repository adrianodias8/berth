// The listening socket and one thread per connection. In a VM the agent
// listens on AF_VSOCK port 2401 (the host's agent.sock is forwarded there);
// for development on a Mac, `--unix <path>` listens on a unix socket instead.
// Darwin's Swift overlay doesn't import <sys/vsock.h>, so sockaddr_vm is laid
// out by hand: it's 12 packed bytes (len, family, reserved u16, port u32,
// cid u32, both in host byte order).
import Darwin
import Foundation
import GuiCore

enum Listen {
    case vsock(port: UInt32)
    case unix(path: String)
}

private let AF_VSOCK_DARWIN: Int32 = 40 // AF_VSOCK in <sys/socket.h>
private let VMADDR_CID_ANY: UInt32 = 0xFFFF_FFFF

/// How long a read may block. A host that opens a connection and goes quiet
/// shouldn't pin a thread for ever; a push's tar keeps bytes flowing.
private let readTimeoutSeconds = 60

func listenSocket(_ where_: Listen) throws -> Int32 {
    switch where_ {
    case .vsock(let port):
        let fd = socket(AF_VSOCK_DARWIN, SOCK_STREAM, 0)
        guard fd >= 0 else { throw GuiError("vsock socket: \(String(cString: strerror(errno)))") }
        var addr = [UInt8](repeating: 0, count: 12)
        addr[0] = 12
        addr[1] = UInt8(AF_VSOCK_DARWIN)
        withUnsafeBytes(of: port.littleEndian) { addr.replaceSubrange(4..<8, with: $0) }
        withUnsafeBytes(of: VMADDR_CID_ANY.littleEndian) { addr.replaceSubrange(8..<12, with: $0) }
        let rc = addr.withUnsafeBytes {
            bind(fd, $0.baseAddress!.assumingMemoryBound(to: sockaddr.self), 12)
        }
        guard rc == 0 else {
            let e = String(cString: strerror(errno))
            close(fd)
            throw GuiError("bind vsock port \(port): \(e)")
        }
        guard Darwin.listen(fd, 16) == 0 else { close(fd); throw GuiError("listen: \(String(cString: strerror(errno)))") }
        return fd
    case .unix(let path):
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw GuiError("unix socket: \(String(cString: strerror(errno)))") }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            close(fd)
            throw GuiError("socket path too long: \(path)")
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        unlink(path)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0 else {
            let e = String(cString: strerror(errno))
            close(fd)
            throw GuiError("bind \(path): \(e)")
        }
        // Only this user may drive the screen.
        chmod(path, 0o600)
        guard Darwin.listen(fd, 16) == 0 else { close(fd); throw GuiError("listen: \(String(cString: strerror(errno)))") }
        return fd
    }
}

/// Accepts for ever, handing each connection to its own thread.
func serve(_ listener: Int32, handler: Handler) -> Never {
    while true {
        let fd = accept(listener, nil, nil)
        if fd < 0 {
            if errno == EINTR || errno == ECONNABORTED { continue }
            log("accept: \(String(cString: strerror(errno)))")
            sleep(1)
            continue
        }
        var on: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        var tv = timeval(tv_sec: readTimeoutSeconds, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        let t = Thread {
            Connection(fd: fd).run(handler)
        }
        t.stackSize = 4 << 20 // deep AX trees recurse
        t.start()
    }
}

final class Connection {
    let fd: Int32
    private(set) var reader: LineReader

    init(fd: Int32) {
        self.fd = fd
        self.reader = LineReader(read: {
            var buf = [UInt8](repeating: 0, count: 64 * 1024)
            while true {
                let n = buf.withUnsafeMutableBytes { recv(fd, $0.baseAddress, $0.count, 0) }
                if n > 0 { return Data(buf[0..<n]) }
                if n == 0 { return Data() }
                if errno == EINTR { continue }
                if errno == EAGAIN || errno == EWOULDBLOCK { throw GuiError("timed out reading from the host") }
                throw GuiError("read: \(String(cString: strerror(errno)))")
            }
        })
    }

    func run(_ handler: Handler) {
        defer { close(fd) }
        let reply: Reply
        let started = Date()
        var verb = "?"
        do {
            let line = try reader.readLine()
            let req = try Request.parse(line)
            verb = req.verb
            reply = handler.handle(req, connection: self)
        } catch let e as GuiError {
            reply = .failure(e.message)
        } catch {
            reply = .failure("\(error)")
        }
        let ms = Int(Date().timeIntervalSince(started) * 1000)
        log("\(verb) \(reply.ok ? "ok" : "error: \(reply.error ?? "")") \(ms) ms")
        writeAll(reply.encoded())
    }

    /// The bytes after the request line; see LineReader.readPayload.
    func readPayload(length: Int?, sink: (Data) throws -> Void) throws {
        try reader.readPayload(length: length, sink: sink)
    }

    private func writeAll(_ data: Data) {
        data.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let n = send(fd, raw.baseAddress! + off, raw.count - off, 0)
                if n > 0 { off += n; continue }
                if n < 0 && errno == EINTR { continue }
                return // the host went away; nothing more to tell it
            }
        }
    }
}

func log(_ message: String) {
    let stamp = ISO8601DateFormatter().string(from: Date())
    FileHandle.standardError.write(Data("\(stamp) \(message)\n".utf8))
}
