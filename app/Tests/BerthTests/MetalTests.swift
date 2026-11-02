import Metal
import XCTest
@testable import Berth

final class MetalTests: XCTestCase {
    private func device() throws -> MTLDevice {
        try XCTUnwrap(MTLCreateSystemDefaultDevice(), "no Metal device")
    }

    override func setUpWithError() throws {
        try XCTSkipIf(MTLCreateSystemDefaultDevice() == nil, "no Metal device")
    }

    /// Copies `slot` out of the ring on the GPU into a shared texture, the
    /// way MetalFrameView does into its private one, and returns the read.
    private func copy(_ ring: Ring, slot: Int, device: MTLDevice) throws -> (MTLTexture, MetalFrameView.SlotRead?) {
        let buffer = try XCTUnwrap(ring.makeBuffer(device), "bytesNoCopy over the mapped ring")
        XCTAssertEqual(buffer.length, ring.length)
        XCTAssertEqual(buffer.contents(), UnsafeMutableRawPointer(mutating: ring.base), "no copy")
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: ring.width,
                                                            height: ring.height, mipmapped: false)
        desc.storageMode = .shared
        let texture = try XCTUnwrap(device.makeTexture(descriptor: desc))
        let commands = try XCTUnwrap(device.makeCommandQueue()?.makeCommandBuffer())
        let read = MetalFrameView.encodeSlotRead(commands, buffer: buffer, ring: ring, slot: slot, into: texture)
        commands.commit()
        commands.waitUntilCompleted()
        XCTAssertNil(commands.error)
        return (texture, read)
    }

    func testSlotReadsBackThePattern() throws {
        let device = try device()
        let fake = try FakeRing(path: tempFilePath("metal"), width: 1920, height: 1080)
        fake.write(slot: 0, pixel: FakeRing.pattern(slot: 0))
        fake.write(slot: 1, pixel: FakeRing.pattern(slot: 1))
        let ring = try Ring.open(fake.path)
        let (texture, read) = try copy(ring, slot: 1, device: device)
        XCTAssertEqual(read?.intact, true)
        for (x, y) in [(0, 0), (1919, 0), (5, 3), (960, 540), (1919, 1079)] {
            var pixel: UInt32 = 0
            texture.getBytes(&pixel, bytesPerRow: 4, from: MTLRegionMake2D(x, y, 1, 1), mipmapLevel: 0)
            XCTAssertEqual(pixel, FakeRing.pattern(slot: 1)(x, y), "pixel \(x),\(y)")
        }
    }

    func testTornCopyIsRejected() throws {
        let device = try device()
        let fake = try FakeRing(path: tempFilePath("torn"), width: 64, height: 32)
        fake.write(slot: 2, pixel: FakeRing.pattern(slot: 2))
        let ring = try Ring.open(fake.path)
        let (_, read) = try copy(ring, slot: 2, device: device)
        let slotRead = try XCTUnwrap(read)
        XCTAssertTrue(slotRead.intact)
        // The daemon rewrote the slot while the GPU was reading it.
        fake.bump(slot: 2)
        XCTAssertFalse(slotRead.intact)
    }

    func testSlotMidWriteIsNotCopied() throws {
        let device = try device()
        let fake = try FakeRing(path: tempFilePath("odd"), width: 64, height: 32)
        fake.put64(RingLayout.offSlotSeq + 8, 5)
        let ring = try Ring.open(fake.path)
        let (_, read) = try copy(ring, slot: 1, device: device)
        XCTAssertNil(read)
    }

    func testShadersCompile() throws {
        let device = try device()
        let library = try device.makeLibrary(source: MetalFrameView.shaders, options: nil)
        for name in ["quad_vs", "frame_fs", "cursor_fs"] { XCTAssertNotNil(library.makeFunction(name: name), name) }
    }
}
