import BerthAtomics
import Foundation
@testable import Berth

/// A surface ring written the way the daemon writes one
/// (docs/display-protocol.md), so the app's reader is tested on the host.
final class FakeRing {
    let path: String
    let width: Int
    let height: Int
    let stride: Int
    let slotBytes: Int
    let length: Int
    let base: UnsafeMutableRawPointer

    init(path: String, width: Int, height: Int, gen: UInt32 = 1,
         magic: String = "BRTHSURF", version: UInt32 = RingLayout.version) throws {
        self.path = path
        self.width = width
        self.height = height
        stride = RingLayout.stride(width: width)
        slotBytes = RingLayout.slotBytes(stride: stride, height: height)
        length = RingLayout.fileBytes(width: width, height: height)
        unlink(path)
        let fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0o600)
        guard fd >= 0 else { throw RingError(message: "open \(path)") }
        defer { close(fd) }
        guard ftruncate(fd, off_t(length)) == 0,
              let map = mmap(nil, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0), map != MAP_FAILED else {
            throw RingError(message: "map \(path)")
        }
        base = map
        let magicBytes = Array(magic.utf8.prefix(8))
        base.copyMemory(from: magicBytes, byteCount: magicBytes.count)
        put32(RingLayout.offVersion, version)
        put32(RingLayout.offGen, gen)
        put32(RingLayout.offWidth, UInt32(width))
        put32(RingLayout.offHeight, UInt32(height))
        put32(RingLayout.offStride, UInt32(stride))
        put32(RingLayout.offFormat, RingLayout.formatBGRA8)
        put32(RingLayout.offSlots, UInt32(RingLayout.slots))
        for i in 0..<RingLayout.slots {
            put64(RingLayout.offSlotOffset + 8 * i, UInt64(RingLayout.slotOffset(i, slotBytes: slotBytes)))
        }
    }

    deinit {
        munmap(base, length)
        unlink(path)
    }

    func put32(_ off: Int, _ v: UInt32) { base.storeBytes(of: v.littleEndian, toByteOffset: off, as: UInt32.self) }
    func put64(_ off: Int, _ v: UInt64) { base.storeBytes(of: v.littleEndian, toByteOffset: off, as: UInt64.self) }

    func slotSeq(_ slot: Int) -> UInt64 { berth_load_acquire_u64(base + RingLayout.offSlotSeq + 8 * slot) }

    /// The seqlock write: odd, the pixels, even again, then `latest`.
    func write(slot: Int, pixel: (Int, Int) -> UInt32) {
        let word = base + RingLayout.offSlotSeq + 8 * slot
        let seq = berth_load_acquire_u64(word)
        berth_store_release_u64(word, seq | 1)
        let start = base + RingLayout.slotOffset(slot, slotBytes: slotBytes)
        for y in 0..<height {
            let row = (start + y * stride).assumingMemoryBound(to: UInt32.self)
            for x in 0..<width { row[x] = pixel(x, y).littleEndian }
        }
        berth_store_release_u64(word, (seq | 1) + 1)
        berth_store_release_u32(base + RingLayout.offLatest, UInt32(slot))
    }

    /// Marks a slot rewritten (its word moves on by 2) without new pixels.
    func bump(slot: Int) {
        let word = base + RingLayout.offSlotSeq + 8 * slot
        berth_store_release_u64(word, berth_load_acquire_u64(word) + 2)
    }

    func setCursor(x: Int32, y: Int32, hotX: Int32, hotY: Int32, w: UInt32, h: UInt32, visible: Bool, seq: UInt64) {
        base.storeBytes(of: x.littleEndian, toByteOffset: RingLayout.offCursorX, as: Int32.self)
        base.storeBytes(of: y.littleEndian, toByteOffset: RingLayout.offCursorY, as: Int32.self)
        base.storeBytes(of: hotX.littleEndian, toByteOffset: RingLayout.offCursorHotX, as: Int32.self)
        base.storeBytes(of: hotY.littleEndian, toByteOffset: RingLayout.offCursorHotY, as: Int32.self)
        put32(RingLayout.offCursorW, w)
        put32(RingLayout.offCursorH, h)
        put32(RingLayout.offCursorVisible, visible ? 1 : 0)
        berth_store_release_u64(base + RingLayout.offCursorSeq, seq)
    }

    /// A pattern that tells slots and positions apart.
    static func pattern(slot: Int) -> (Int, Int) -> UInt32 {
        { x, y in UInt32(truncatingIfNeeded: (slot << 24) | ((y & 0xfff) << 12) | (x & 0xfff)) }
    }
}

/// A short socket path in the temp folder (sun_path holds 104 bytes).
func tempSocketPath(_ name: String) -> String {
    let dir = FileManager.default.temporaryDirectory.path
    return (dir as NSString).appendingPathComponent("b-\(name)-\(UInt32.random(in: 0...UInt32.max) % 100000).sock")
}

func tempFilePath(_ name: String) -> String {
    (FileManager.default.temporaryDirectory.path as NSString)
        .appendingPathComponent("berth-\(name)-\(UUID().uuidString.prefix(8))")
}

/// Polls until `condition` holds or `timeout` passes.
func waitUntil(_ timeout: TimeInterval = 5, _ condition: () -> Bool) async -> Bool {
    let deadline = Date().addingTimeInterval(timeout)
    while Date() < deadline {
        if condition() { return true }
        try? await Task.sleep(for: .milliseconds(10))
    }
    return condition()
}
