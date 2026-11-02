import XCTest
@testable import Berth

final class RingTests: XCTestCase {
    func testLayoutFor1080p() {
        XCTAssertEqual(RingLayout.stride(width: 1920), 7680)
        XCTAssertEqual(RingLayout.stride(width: 1366), 5632)
        XCTAssertEqual(RingLayout.stride(width: 640) % 256, 0)
        XCTAssertEqual(RingLayout.slotBytes(stride: 7680, height: 1080) % 16384, 0)
        XCTAssertEqual(RingLayout.slotBytes(stride: 7680, height: 1080), 507 * 16384)
    }

    func testFixtureParses() throws {
        let fake = try FakeRing(path: tempFilePath("ring"), width: 1920, height: 1080, gen: 4)
        fake.write(slot: 0, pixel: FakeRing.pattern(slot: 0))
        fake.write(slot: 1, pixel: FakeRing.pattern(slot: 1))
        fake.write(slot: 1, pixel: FakeRing.pattern(slot: 1))
        fake.setCursor(x: 10, y: 20, hotX: 1, hotY: 2, w: 24, h: 24, visible: true, seq: 7)

        let ring = try Ring.open(fake.path)
        XCTAssertEqual(ring.gen, 4)
        XCTAssertEqual(ring.width, 1920)
        XCTAssertEqual(ring.height, 1080)
        XCTAssertEqual(ring.stride, 7680)
        XCTAssertEqual(ring.slotOffsets, [16384, 16384 + 507 * 16384, 16384 + 2 * 507 * 16384])
        XCTAssertEqual(ring.cursorOffset, 16384 + 3 * 507 * 16384)
        XCTAssertEqual(ring.length, ring.cursorOffset + 256 * 256 * 4)
        XCTAssertEqual(ring.slotSeq(0), 2)
        XCTAssertEqual(ring.slotSeq(1), 4)
        XCTAssertEqual(ring.slotSeq(2), 0)
        XCTAssertEqual(ring.latest, 1)
        XCTAssertEqual(ring.cursor, RingCursor(x: 10, y: 20, hotX: 1, hotY: 2, width: 24, height: 24, visible: true, seq: 7))
        // The pixel at (5, 3) of slot 1, read straight out of the mapping.
        let pixel = ring.base.load(fromByteOffset: ring.slotOffsets[1] + 3 * ring.stride + 5 * 4, as: UInt32.self)
        XCTAssertEqual(pixel, FakeRing.pattern(slot: 1)(5, 3))
    }

    func testBadMagicIsRefused() throws {
        let fake = try FakeRing(path: tempFilePath("magic"), width: 64, height: 64, magic: "NOTARING")
        XCTAssertThrowsError(try Ring.open(fake.path)) { error in
            XCTAssertTrue("\(error.localizedDescription)".contains("not a berth surface ring"))
        }
    }

    func testOtherVersionIsRefused() throws {
        let fake = try FakeRing(path: tempFilePath("version"), width: 64, height: 64, version: 2)
        XCTAssertThrowsError(try Ring.open(fake.path)) { error in
            XCTAssertTrue("\(error.localizedDescription)".contains("version 2"))
        }
    }

    func testSlotOutsideTheFileIsRefused() throws {
        let fake = try FakeRing(path: tempFilePath("slot"), width: 64, height: 64)
        fake.put64(RingLayout.offSlotOffset + 16, UInt64(fake.length))
        XCTAssertThrowsError(try Ring.open(fake.path))
    }

    func testSnapshotCopiesTheLatestSlot() throws {
        let fake = try FakeRing(path: tempFilePath("snap"), width: 32, height: 16)
        fake.write(slot: 2, pixel: { _, _ in 0xff00_00ff }) // BGRA in memory: ff 00 00 ff, red
        let ring = try Ring.open(fake.path)
        let image = try XCTUnwrap(ring.snapshot())
        XCTAssertEqual(image.width, 32)
        XCTAssertEqual(image.height, 16)
        XCTAssertEqual(image.bytesPerRow, ring.stride)
        let data = try XCTUnwrap(image.dataProvider?.data as Data?)
        XCTAssertEqual(Array(data.prefix(4)), [0xff, 0x00, 0x00, 0xff])
    }

    func testSnapshotSkipsASlotMidWrite() throws {
        let fake = try FakeRing(path: tempFilePath("mid"), width: 32, height: 16)
        fake.write(slot: 0, pixel: { _, _ in 1 })
        fake.put64(RingLayout.offSlotSeq, 3) // odd: being written
        let ring = try Ring.open(fake.path)
        XCTAssertNil(ring.snapshot())
    }
}
