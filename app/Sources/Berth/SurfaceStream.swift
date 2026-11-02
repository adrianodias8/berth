import BerthAtomics
import CoreGraphics
import Foundation
import Metal

// A live surface from the daemon (docs/display-protocol.md): the app sends
// `{"cmd":"display",…}` on the control socket, the reply names a ring file
// under ~/.berth/run/surfaces that the daemon keeps the surface's newest
// frames in, and the connection then carries doorbell lines one way and
// input lines the other. The pixels never cross the socket: the ring is
// mapped here and handed to Metal without a copy.

/// Where the ring's fields live. Little-endian, offsets in bytes from the
/// start of the file; the daemon's src/gpu/ring.rs writes the same layout.
enum RingLayout {
    static let magic = Array("BRTHSURF".utf8)
    static let version: UInt32 = 1
    /// The header's size, and the alignment of every slot (a page on Apple
    /// silicon, so Metal can wrap a slot without a copy).
    static let headerBytes = 16384
    static let slotAlign = 16384
    static let strideAlign = 256
    static let slots = 3
    /// The cursor image after the slots: 256x256 BGRA, 1024 bytes a row.
    static let cursorSide = 256
    static let cursorBytes = cursorSide * cursorSide * 4
    /// The header's `format` for "bgra8": B, G, R, A in memory, alpha
    /// undefined (XRGB8888 from the guest).
    static let formatBGRA8: UInt32 = 1

    static let offVersion = 8
    static let offGen = 12
    static let offWidth = 16
    static let offHeight = 20
    static let offStride = 24
    static let offFormat = 28
    static let offSlots = 32
    /// u64 each, 3 of them.
    static let offSlotOffset = 40
    /// u64 seqlock words, 3 of them: odd while the slot is being written.
    static let offSlotSeq = 64
    /// u32: the index of the newest complete slot.
    static let offLatest = 88
    static let offCursorX = 96
    static let offCursorY = 100
    static let offCursorHotX = 104
    static let offCursorHotY = 108
    static let offCursorW = 112
    static let offCursorH = 116
    static let offCursorVisible = 120
    static let offCursorSeq = 128

    static func alignUp(_ n: Int, _ to: Int) -> Int { (n + to - 1) / to * to }
    static func stride(width: Int) -> Int { alignUp(width * 4, strideAlign) }
    static func slotBytes(stride: Int, height: Int) -> Int { alignUp(stride * height, slotAlign) }
    static func slotOffset(_ index: Int, slotBytes: Int) -> Int { headerBytes + index * slotBytes }
    static func cursorOffset(slotBytes: Int) -> Int { headerBytes + slots * slotBytes }
    static func fileBytes(width: Int, height: Int) -> Int {
        cursorOffset(slotBytes: slotBytes(stride: stride(width: width), height: height)) + cursorBytes
    }
}

struct RingError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

/// The guest's pointer image and where it is, from the ring's header.
struct RingCursor: Equatable {
    var x: Int, y: Int, hotX: Int, hotY: Int
    var width: Int, height: Int
    var visible: Bool
    var seq: UInt64
}

/// One generation of a surface's ring file, mapped read-only. A resize
/// makes a new file (a new gen); this one stays valid until it's dropped.
final class Ring: @unchecked Sendable {
    let path: String
    let gen: UInt32
    let width: Int
    let height: Int
    let stride: Int
    let slotBytes: Int
    let slotOffsets: [Int]
    let cursorOffset: Int
    let length: Int
    let base: UnsafeRawPointer

    /// Maps the file and checks its header: magic, version, format and
    /// that every slot fits, aligned, inside it.
    static func open(_ path: String) throws -> Ring {
        let fd = Darwin.open(path, O_RDONLY | O_CLOEXEC)
        guard fd >= 0 else { throw RingError(message: "open \(path): \(String(cString: strerror(errno)))") }
        defer { close(fd) }
        var st = stat()
        guard fstat(fd, &st) == 0 else { throw RingError(message: "stat \(path): \(String(cString: strerror(errno)))") }
        let length = Int(st.st_size)
        guard length >= RingLayout.headerBytes else { throw RingError(message: "\(path): too short for a ring") }
        guard let map = mmap(nil, length, PROT_READ, MAP_SHARED, fd, 0), map != MAP_FAILED else {
            throw RingError(message: "mmap \(path): \(String(cString: strerror(errno)))")
        }
        do {
            return try Ring(path: path, base: UnsafeRawPointer(map), length: length)
        } catch {
            munmap(map, length)
            throw error
        }
    }

    private init(path: String, base: UnsafeRawPointer, length: Int) throws {
        func u32(_ off: Int) -> UInt32 { UInt32(littleEndian: base.loadUnaligned(fromByteOffset: off, as: UInt32.self)) }
        func u64(_ off: Int) -> UInt64 { UInt64(littleEndian: base.loadUnaligned(fromByteOffset: off, as: UInt64.self)) }
        guard Array(UnsafeRawBufferPointer(start: base, count: 8)) == RingLayout.magic else {
            throw RingError(message: "\(path): not a berth surface ring")
        }
        guard u32(RingLayout.offVersion) == RingLayout.version else {
            throw RingError(message: "\(path): ring version \(u32(RingLayout.offVersion)), this app reads \(RingLayout.version)")
        }
        guard u32(RingLayout.offFormat) == RingLayout.formatBGRA8 else {
            throw RingError(message: "\(path): pixel format \(u32(RingLayout.offFormat)) isn't bgra8")
        }
        guard Int(u32(RingLayout.offSlots)) == RingLayout.slots else {
            throw RingError(message: "\(path): \(u32(RingLayout.offSlots)) slots, expected \(RingLayout.slots)")
        }
        let width = Int(u32(RingLayout.offWidth)), height = Int(u32(RingLayout.offHeight))
        let stride = Int(u32(RingLayout.offStride))
        guard width > 0, height > 0, width <= 16384, height <= 16384,
              stride >= width * 4, stride % RingLayout.strideAlign == 0 else {
            throw RingError(message: "\(path): bad geometry \(width)x\(height) stride \(stride)")
        }
        let slotBytes = RingLayout.slotBytes(stride: stride, height: height)
        let offsets = (0..<RingLayout.slots).map { Int(u64(RingLayout.offSlotOffset + 8 * $0)) }
        for off in offsets {
            guard off >= RingLayout.headerBytes, off % RingLayout.slotAlign == 0, off + stride * height <= length else {
                throw RingError(message: "\(path): slot at \(off) is outside the file or misaligned")
            }
        }
        let cursorOffset = RingLayout.cursorOffset(slotBytes: slotBytes)
        guard cursorOffset + RingLayout.cursorBytes <= length else {
            throw RingError(message: "\(path): no room for the cursor image")
        }
        self.path = path
        self.base = base
        self.length = length
        self.gen = u32(RingLayout.offGen)
        self.width = width
        self.height = height
        self.stride = stride
        self.slotBytes = slotBytes
        self.slotOffsets = offsets
        self.cursorOffset = cursorOffset
    }

    deinit { munmap(UnsafeMutableRawPointer(mutating: base), length) }

    /// The slot's seqlock word: odd while the daemon writes it, and changed
    /// once it has. A copy is good if the word was even and the same before
    /// and after.
    func slotSeq(_ index: Int) -> UInt64 {
        UInt64(littleEndian: berth_load_acquire_u64(base + RingLayout.offSlotSeq + 8 * index))
    }

    /// The newest complete slot.
    var latest: Int {
        Int(UInt32(littleEndian: berth_load_acquire_u32(base + RingLayout.offLatest))) % RingLayout.slots
    }

    var cursor: RingCursor {
        func i32(_ off: Int) -> Int { Int(Int32(littleEndian: base.loadUnaligned(fromByteOffset: off, as: Int32.self))) }
        func u32(_ off: Int) -> Int { Int(UInt32(littleEndian: base.loadUnaligned(fromByteOffset: off, as: UInt32.self))) }
        let seq = UInt64(littleEndian: berth_load_acquire_u64(base + RingLayout.offCursorSeq))
        return RingCursor(
            x: i32(RingLayout.offCursorX), y: i32(RingLayout.offCursorY),
            hotX: i32(RingLayout.offCursorHotX), hotY: i32(RingLayout.offCursorHotY),
            width: min(u32(RingLayout.offCursorW), RingLayout.cursorSide),
            height: min(u32(RingLayout.offCursorH), RingLayout.cursorSide),
            visible: u32(RingLayout.offCursorVisible) != 0, seq: seq)
    }

    /// The whole mapping as one shared buffer, with no copy and no
    /// deallocator: whoever uses it keeps this Ring alive until the GPU is
    /// done (a command buffer's completion handler holding it), and the
    /// munmap happens in deinit.
    func makeBuffer(_ device: MTLDevice) -> MTLBuffer? {
        device.makeBuffer(bytesNoCopy: UnsafeMutableRawPointer(mutating: base), length: length,
                          options: .storageModeShared, deallocator: nil)
    }

    /// A CPU copy of the newest slot as an image, or nil when the daemon
    /// was writing it meanwhile (try again next time). For the Agents
    /// peek, which shows a frame or two a second and needs no Metal.
    func snapshot() -> CGImage? {
        let slot = latest
        let before = slotSeq(slot)
        guard before != 0, before & 1 == 0 else { return nil }
        let bytes = Data(bytes: base + slotOffsets[slot], count: stride * height)
        berth_fence_acquire()
        guard slotSeq(slot) == before, let provider = CGDataProvider(data: bytes as CFData) else { return nil }
        return CGImage(width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: stride,
                       space: CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB(),
                       bitmapInfo: CGBitmapInfo(rawValue: CGBitmapInfo.byteOrder32Little.rawValue
                           | CGImageAlphaInfo.noneSkipFirst.rawValue),
                       provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
    }
}

/// The reply line to a display request.
struct RingInfo: Decodable, Equatable {
    var ok: Bool
    var error: String?
    var transports: [String]?
    var ring: String?
    var gen: UInt32?
    var width: Int?
    var height: Int?
    var stride: Int?
    var format: String?
    var slots: Int?
    /// Whether the surface has had a frame yet.
    var live: Bool?
}

/// One doorbell line.
struct Doorbell: Decodable, Equatable {
    var ev: String
    var slot: Int?
    var seq: UInt64?
    var damage: [[Int]]?
    var ring: String?
    var gen: UInt32?
    var width: Int?
    var height: Int?
    var error: String?
}

final class SurfaceStream: @unchecked Sendable {
    /// A headless session's own surface, or a VM scanout (a desktop
    /// session on card0, whose session JSON carries "scanout").
    enum Source: Hashable {
        case session(String)
        case scanout(Int)
    }

    enum State: Equatable {
        case connecting
        case live
        /// Gave up after `maxFailures` tries in a row: a daemon from before
        /// display streams, the VM stopped, or the session ended.
        case unavailable(String)
    }

    /// What the newest doorbell said. `serial` counts every new frame seen
    /// here, connects and resizes included, so a view redraws on it even
    /// when the daemon's `seq` starts over. Cursor doorbells only move
    /// `cursorSeq`: the cursor's state is read from the ring's header.
    struct Pending: Equatable {
        var slot = 0
        var seq: UInt64 = 0
        var cursorSeq: UInt64 = 0
        var serial: UInt64 = 0
    }

    static let maxFailures = 3

    let machine: String
    let source: Source
    let maxFPS: Int
    let socketPath: String
    /// For a scanout: the desktop session input goes to. The daemon drops
    /// input on a stream that names no session.
    let inputSession: String?
    /// Waits between failed tries; the last repeats.
    var backoff: [Duration] = [.milliseconds(250), .milliseconds(500), .seconds(1)]
    /// Called on the main queue.
    var onState: ((State) -> Void)?

    private let lock = NSLock()
    private var _state: State = .connecting
    private var _ring: Ring?
    private var _pending = Pending()
    private var connection: LineStream?
    private var controlOn = false
    private var task: Task<Void, Never>?

    init(machine: String, source: Source, inputSession: String? = nil, maxFPS: Int = 60,
         socketPath: String = BerthPaths.controlSocket) {
        self.machine = machine
        self.source = source
        self.inputSession = inputSession
        self.maxFPS = max(1, min(maxFPS, 120))
        self.socketPath = socketPath
    }

    convenience init(target: DisplayTarget, maxFPS: Int = 60, socketPath: String = BerthPaths.controlSocket) {
        self.init(machine: target.machine, source: target.source, inputSession: target.session, maxFPS: maxFPS,
                  socketPath: socketPath)
    }

    deinit { cancel() }

    var state: State { lock.withLock { _state } }
    var ring: Ring? { lock.withLock { _ring } }
    var pending: Pending { lock.withLock { _pending } }
    /// The ring and doorbell read together, so the slot belongs to the ring.
    var current: (ring: Ring?, pending: Pending) { lock.withLock { (_ring, _pending) } }

    /// The request line, as docs/display-protocol.md spells it.
    /// A scanout's request names `inputSession` too, so input reaches it.
    static func request(machine: String, source: Source, maxFPS: Int, inputSession: String? = nil) -> [String: Any] {
        var obj: [String: Any] = ["cmd": "display", "machine": machine, "max_fps": maxFPS]
        switch source {
        case let .session(session): obj["session"] = session
        case let .scanout(n):
            obj["scanout"] = n
            if let inputSession { obj["session"] = inputSession }
        }
        return obj
    }

    func start() {
        lock.withLock {
            guard task == nil else { return }
            task = Task.detached { [weak self] in await self?.loop() }
        }
    }

    func cancel() {
        let (task, conn) = lock.withLock { () -> (Task<Void, Never>?, LineStream?) in
            defer { self.task = nil; connection = nil }
            return (self.task, connection)
        }
        task?.cancel()
        conn?.cancel()
    }

    /// Drops the connection; the loop connects again straight away.
    func reconnect() {
        lock.withLock { connection }?.cancel()
    }

    /// An input line for the guest; dropped when there is no connection.
    /// The daemon forwards it only while control is on, and control is per
    /// connection: a reconnect turns it back on if it was.
    func send(_ event: InputEvent) {
        let conn = lock.withLock { () -> LineStream? in
            if case let .control(on) = event { controlOn = on }
            return connection
        }
        conn?.send(event.json)
    }

    private func set(_ state: State) {
        let changed = lock.withLock { () -> Bool in
            defer { _state = state }
            return _state != state
        }
        guard changed, let onState else { return }
        DispatchQueue.main.async { onState(state) }
    }

    private func loop() async {
        var failures = 0
        var lastError = "no display stream"
        while !Task.isCancelled {
            switch await session() {
            case .wasLive:
                // A stream that ended goes again at once, but never in a
                // tight loop against a daemon that keeps closing.
                failures = 0
                set(.connecting)
                try? await Task.sleep(for: backoff[0])
                continue
            case let .failed(message):
                lastError = message
                failures += 1
            }
            if Task.isCancelled { return }
            if failures >= Self.maxFailures {
                set(.unavailable(lastError))
                return
            }
            set(.connecting)
            try? await Task.sleep(for: backoff[min(failures - 1, backoff.count - 1)])
        }
    }

    private enum Outcome {
        case wasLive
        case failed(String)
    }

    /// One connection, from the request to its close.
    private func session() async -> Outcome {
        let request = Self.request(machine: machine, source: source, maxFPS: maxFPS, inputSession: inputSession)
        let path = socketPath
        let conn: LineStream
        do {
            conn = try await offPool { try Control.open(request, socketPath: path) }
        } catch {
            return .failed(error.localizedDescription)
        }
        let keep = lock.withLock { () -> Bool in
            guard task != nil, !(task?.isCancelled ?? true) else { return false }
            connection = conn
            return true
        }
        guard keep else {
            conn.cancel()
            return .failed("cancelled")
        }
        defer {
            conn.cancel()
            lock.withLock { if connection === conn { connection = nil } }
        }
        var lines = conn.lines.makeAsyncIterator()
        let reply: RingInfo
        do {
            guard let line = try await lines.next() else { return .failed("the daemon closed the connection") }
            reply = try JSONDecoder().decode(RingInfo.self, from: Data(line.utf8))
        } catch {
            return .failed("no display reply: \(error.localizedDescription)")
        }
        guard reply.ok else { return .failed(reply.error ?? "display refused") }
        guard reply.transports?.contains("ring") ?? true else {
            return .failed("no ring transport offered")
        }
        // Not live yet (a session's capture still starting, a scanout that's
        // off): no ring until a resize doorbell brings one. A connection that
        // ends before then counts as a failure, so a surface that never
        // comes still falls back to screenshots.
        var hadRing = false
        if let path = reply.ring {
            do {
                let ring = try Ring.open(path)
                lock.withLock {
                    _ring = ring
                    _pending.slot = ring.latest
                    _pending.serial += 1
                }
                hadRing = true
            } catch {
                return .failed(error.localizedDescription)
            }
        } else if reply.live != false {
            return .failed("no ring offered")
        }
        if lock.withLock({ controlOn }) { conn.send(InputEvent.control(on: true).json) }
        if hadRing { set(.live) }
        let ended = { hadRing ? Outcome.wasLive : Outcome.failed("the surface ended before its first frame") }
        do {
            while let line = try await lines.next() {
                guard let bell = try? JSONDecoder().decode(Doorbell.self, from: Data(line.utf8)) else { continue }
                switch bell.ev {
                case "frame":
                    guard let slot = bell.slot, (0..<RingLayout.slots).contains(slot) else { continue }
                    lock.withLock {
                        _pending.slot = slot
                        _pending.seq = bell.seq ?? _pending.seq + 1
                        _pending.serial += 1
                    }
                case "cursor":
                    lock.withLock { _pending.cursorSeq = bell.seq ?? _pending.cursorSeq + 1 }
                case "resize":
                    // The new gen's file; the old mapping stays alive for
                    // whatever still holds it (a blit in flight).
                    guard let path = bell.ring, let ring = try? Ring.open(path) else { continue }
                    lock.withLock {
                        _ring = ring
                        _pending.slot = ring.latest
                        _pending.serial += 1
                    }
                    if !hadRing {
                        hadRing = true
                        set(.live)
                    }
                case "end":
                    return ended()
                default:
                    continue // keepalive, and events newer than this app
                }
            }
        } catch {}
        return ended()
    }
}

extension DisplayTarget {
    /// "dev" from "linux:dev".
    var machine: String { target.split(separator: ":", maxSplits: 1).last.map(String.init) ?? target }

    /// A desktop session is the VM's scanout; every other session has its
    /// own surface.
    var source: SurfaceStream.Source { scanout.map { .scanout($0) } ?? .session(session) }
}
