import XCTest
@testable import Berth

/// The daemon's end of a display connection, on a temp socket.
final class FakeDisplayServer: @unchecked Sendable {
    let path: String
    private var fd: Int32

    init(path: String) throws {
        self.path = path
        unlink(path)
        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        withUnsafeMutableBytes(of: &addr.sun_path) { buf in
            buf.copyBytes(from: bytes)
            buf[bytes.count] = 0
        }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard rc == 0, listen(fd, 4) == 0 else { throw RingError(message: "bind \(path): \(errno)") }
    }

    deinit { stop() }

    func accept() throws -> FakeConn {
        let conn = Darwin.accept(fd, nil, nil)
        guard conn >= 0 else { throw RingError(message: "accept: \(errno)") }
        return FakeConn(fd: conn)
    }

    /// No more connections: connects fail from now on.
    func stop() {
        guard fd >= 0 else { return }
        close(fd)
        fd = -1
        unlink(path)
    }
}

final class FakeConn: @unchecked Sendable {
    private let fd: Int32
    private var buffer = Data()

    init(fd: Int32) {
        self.fd = fd
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
    }

    deinit { Darwin.close(fd) }

    func readLine() -> String? {
        while true {
            if let newline = buffer.firstIndex(of: 0x0a) {
                let line = String(decoding: buffer[buffer.startIndex..<newline], as: UTF8.self)
                buffer.removeSubrange(buffer.startIndex...newline)
                return line
            }
            var chunk = [UInt8](repeating: 0, count: 4096)
            let n = read(fd, &chunk, chunk.count)
            guard n > 0 else { return nil }
            buffer.append(contentsOf: chunk[0..<n])
        }
    }

    func send(_ lines: String...) {
        let data = Array(lines.map { $0 + "\n" }.joined().utf8)
        _ = data.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
    }

    func close() { Darwin.shutdown(fd, SHUT_RDWR) }
}

func jsonObject(_ line: String) -> NSDictionary? {
    (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? NSDictionary
}

final class SurfaceStreamTests: XCTestCase {
    func testASurfaceNotLiveYetWaitsForItsResize() async throws {
        let socketPath = tempSocketPath("notlive")
        let server = try FakeDisplayServer(path: socketPath)
        let ring = try FakeRing(path: tempFilePath("late"), width: 64, height: 32, gen: 4)
        let stream = SurfaceStream(machine: "dev", source: .session("s-new"), socketPath: socketPath)
        stream.backoff = [.milliseconds(20)]
        stream.start()
        defer { stream.cancel() }

        let conn = try await offPool { try server.accept() }
        _ = await offPool { conn.readLine() }
        conn.send(#"{"ok":true,"transports":["ring"],"ring":null,"gen":0,"width":0,"height":0,"stride":0,"format":"bgra8","slots":3,"live":false}"#,
                  #"{"ev":"keepalive"}"#)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(stream.state, .connecting)
        XCTAssertNil(stream.ring)

        conn.send(#"{"ev":"resize","ring":"\#(ring.path)","gen":4,"width":64,"height":32,"stride":256,"format":"bgra8","slots":3,"live":true}"#,
                  #"{"ev":"frame","slot":0,"seq":5,"damage":[[0,0,64,32]]}"#)
        let live = await waitUntil { stream.state == .live && stream.pending.seq == 5 }
        XCTAssertTrue(live, "pending \(stream.pending), state \(stream.state)")
        XCTAssertEqual(stream.ring?.gen, 4)
    }

    func testASurfaceThatEndsBeforeItsFirstFrameIsAFailure() async throws {
        let socketPath = tempSocketPath("neverlive")
        let server = try FakeDisplayServer(path: socketPath)
        let stream = SurfaceStream(machine: "dev", source: .session("s-gone"), socketPath: socketPath)
        stream.backoff = [.milliseconds(10)]
        stream.start()
        defer { stream.cancel() }
        for _ in 0..<SurfaceStream.maxFailures {
            let conn = try await offPool { try server.accept() }
            _ = await offPool { conn.readLine() }
            conn.send(#"{"ok":true,"transports":["ring"],"ring":null,"live":false}"#,
                      #"{"ev":"end","error":"session ended"}"#)
            conn.close()
        }
        let gaveUp = await waitUntil {
            if case .unavailable = stream.state { return true }
            return false
        }
        XCTAssertTrue(gaveUp, "state \(stream.state)")
    }

    func testStreamsFramesResizesForwardsInputAndGivesUp() async throws {
        let socketPath = tempSocketPath("disp")
        let server = try FakeDisplayServer(path: socketPath)
        let first = try FakeRing(path: tempFilePath("gen1"), width: 64, height: 32, gen: 1)
        let second = try FakeRing(path: tempFilePath("gen2"), width: 128, height: 64, gen: 2)
        for slot in 0..<3 { first.write(slot: slot, pixel: FakeRing.pattern(slot: slot)) }

        let stream = SurfaceStream(machine: "dev", source: .session("s-x"), maxFPS: 30, socketPath: socketPath)
        stream.backoff = [.milliseconds(20)]
        stream.start()
        defer { stream.cancel() }

        let conn = try await offPool { try server.accept() }
        let request = await offPool { conn.readLine() }
        XCTAssertEqual(jsonObject(request ?? ""),
                       jsonObject(#"{"cmd":"display","machine":"dev","session":"s-x","max_fps":30}"#))

        conn.send(#"{"ok":true,"transports":["ring"],"ring":"\#(first.path)","gen":1,"width":64,"height":32,"stride":256,"format":"bgra8","slots":3,"live":true}"#,
                  #"{"ev":"frame","slot":0,"seq":1,"damage":[[0,0,64,32]]}"#,
                  #"{"ev":"keepalive"}"#,
                  #"{"ev":"frame","slot":1,"seq":2,"damage":[[1,1,2,2]]}"#,
                  #"{"ev":"cursor","seq":9}"#,
                  #"{"ev":"frame","slot":2,"seq":3,"damage":[[0,0,8,8]]}"#)
        let reached = await waitUntil { stream.state == .live && stream.pending.seq == 3 }
        XCTAssertTrue(reached, "pending \(stream.pending), state \(stream.state)")
        XCTAssertEqual(stream.pending.slot, 2)
        XCTAssertEqual(stream.pending.cursorSeq, 9)
        XCTAssertEqual(stream.ring?.gen, 1)
        XCTAssertEqual(stream.ring?.width, 64)

        conn.send(#"{"ev":"resize","ring":"\#(second.path)","gen":2,"width":128,"height":64,"stride":512}"#)
        let remapped = await waitUntil { stream.ring?.gen == 2 }
        XCTAssertTrue(remapped)
        XCTAssertEqual(stream.ring?.width, 128)
        XCTAssertEqual(stream.ring?.stride, 512)

        let events: [InputEvent] = [.control(on: true), .pointer(x: 10, y: 20), .button(272, down: true),
                                    .key(code: 30, down: true), .scroll(dx: 0, dy: 15, discrete: true), .releaseAll]
        for event in events { stream.send(event) }
        for event in events {
            let line = await offPool { conn.readLine() }
            XCTAssertEqual(line, event.json, "verbatim")
        }

        // The daemon goes away: the stream connects again...
        conn.close()
        let again = try await offPool { try server.accept() }
        let retried = await offPool { again.readLine() }
        XCTAssertNotNil(retried.flatMap(jsonObject))
        // ...that one closes with no reply, and after that nothing listens:
        // three failures in a row and the stream gives up.
        again.close()
        server.stop()
        let gaveUp = await waitUntil {
            if case .unavailable = stream.state { return true }
            return false
        }
        XCTAssertTrue(gaveUp, "state \(stream.state)")
    }

    func testControlIsTurnedBackOnAfterAReconnect() async throws {
        let socketPath = tempSocketPath("ctl")
        let server = try FakeDisplayServer(path: socketPath)
        let ring = try FakeRing(path: tempFilePath("ctl"), width: 16, height: 16)
        let reply = #"{"ok":true,"transports":["ring"],"ring":"\#(ring.path)","gen":1,"width":16,"height":16,"stride":256,"format":"bgra8","slots":3,"live":true}"#
        let stream = SurfaceStream(machine: "dev", source: .session("s-x"), socketPath: socketPath)
        stream.backoff = [.milliseconds(10)]
        stream.start()
        defer { stream.cancel() }

        let first = try await offPool { try server.accept() }
        _ = await offPool { first.readLine() }
        first.send(reply)
        _ = await waitUntil { stream.state == .live }
        stream.send(.control(on: true))
        let on = await offPool { first.readLine() }
        XCTAssertEqual(on, InputEvent.control(on: true).json)
        first.close()

        let second = try await offPool { try server.accept() }
        _ = await offPool { second.readLine() }
        second.send(reply)
        let resent = await offPool { second.readLine() }
        XCTAssertEqual(resent, InputEvent.control(on: true).json)
    }

    func testRefusedReplyCountsAsAFailure() async throws {
        let socketPath = tempSocketPath("refuse")
        let server = try FakeDisplayServer(path: socketPath)
        let stream = SurfaceStream(machine: "dev", source: .scanout(0), socketPath: socketPath)
        stream.backoff = [.milliseconds(10)]
        stream.start()
        defer { stream.cancel() }
        for _ in 0..<SurfaceStream.maxFailures {
            let conn = try await offPool { try server.accept() }
            let request = await offPool { conn.readLine() }
            XCTAssertEqual(jsonObject(request ?? ""), jsonObject(#"{"cmd":"display","machine":"dev","scanout":0,"max_fps":60}"#))
            conn.send(#"{"ok":false,"error":"no live surface"}"#)
            conn.close()
        }
        let gaveUp = await waitUntil { stream.state == .unavailable("no live surface") }
        XCTAssertTrue(gaveUp, "state \(stream.state)")
    }

    func testOldDaemonWithNoDisplayCommandFallsBack() async throws {
        // A daemon from before display streams can't parse the request and
        // drops the connection without a reply.
        let socketPath = tempSocketPath("old")
        let server = try FakeDisplayServer(path: socketPath)
        let stream = SurfaceStream(machine: "dev", source: .session("s-1"), socketPath: socketPath)
        stream.backoff = [.milliseconds(10)]
        stream.start()
        defer { stream.cancel() }
        for _ in 0..<SurfaceStream.maxFailures {
            let conn = try await offPool { try server.accept() }
            _ = await offPool { conn.readLine() }
            conn.close()
        }
        let gaveUp = await waitUntil {
            if case .unavailable = stream.state { return true }
            return false
        }
        XCTAssertTrue(gaveUp)
    }

    func testASilentDaemonEndsTheLines() async throws {
        // Connected, request read, then nothing: not even the keepalive.
        let socketPath = tempSocketPath("hung")
        let server = try FakeDisplayServer(path: socketPath)
        let stream = try await offPool {
            try Control.open(SurfaceStream.request(machine: "dev", source: .session("s"), maxFPS: 60),
                             socketPath: socketPath, timeout: 0.2)
        }
        defer { stream.cancel() }
        let conn = try await offPool { try server.accept() }
        _ = await offPool { conn.readLine() }
        let started = ContinuousClock.now
        var lines = stream.lines.makeAsyncIterator()
        do {
            _ = try await lines.next()
            XCTFail("a line from a silent daemon")
        } catch {
            XCTAssertLessThan(ContinuousClock.now - started, .seconds(2))
        }
        withExtendedLifetime(conn) {}
    }

    func testTargetPicksTheSource() {
        XCTAssertEqual(DisplayTarget(target: "linux:dev", session: "s-x").source, .session("s-x"))
        XCTAssertEqual(DisplayTarget(target: "linux:dev", session: "s-d", scanout: 0).source, .scanout(0))
        XCTAssertEqual(DisplayTarget(target: "linux:dev", session: "s-x").machine, "dev")
        XCTAssertEqual(DisplayBackend.preferred(for: DisplayTarget(target: "linux:dev", session: "s")), .stream(maxFPS: 60))
        XCTAssertEqual(DisplayBackend.preferred(for: DisplayTarget(target: "mac:vm", session: "s")), .screenshots(every: .seconds(2)))
    }

    func testRestoredTargetsWithoutTheNewFieldsDecode() throws {
        let target = try JSONDecoder().decode(DisplayTarget.self, from: Data(#"{"target":"linux:dev","session":"s-x"}"#.utf8))
        XCTAssertEqual(target, DisplayTarget(target: "linux:dev", session: "s-x"))
    }

    func testSessionsJSONCarriesScanoutAndRenderer() throws {
        let json = #"{"session":"s-d","target":"linux:dev","scanout":0,"renderer":"gles2"}"#
        let session = try JSONDecoder().decode(GuiSession.self, from: Data(json.utf8))
        XCTAssertEqual(session.display, DisplayTarget(target: "linux:dev", session: "s-d", scanout: 0, renderer: "gles2"))
    }
}
