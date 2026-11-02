import Carbon.HIToolbox
import XCTest
@testable import Berth

final class GuestInputTests: XCTestCase {
    func testKeyCodes() {
        XCTAssertEqual(KeyMap.evdev(macKeyCode: kVK_ANSI_A), 30)
        XCTAssertEqual(KeyMap.evdev(macKeyCode: kVK_Return), 28)
        XCTAssertEqual(KeyMap.evdev(macKeyCode: kVK_Shift), 42)
        XCTAssertEqual(KeyMap.evdev(macKeyCode: kVK_Command), 125)
        XCTAssertEqual(KeyMap.evdev(macKeyCode: kVK_LeftArrow), 105)
        XCTAssertEqual(KeyMap.evdev(macKeyCode: kVK_F1), 59)
        XCTAssertEqual(KeyMap.evdev(macKeyCode: kVK_F12), 88)
        XCTAssertEqual(KeyMap.evdev(macKeyCode: kVK_ANSI_KeypadEnter), 96)
        XCTAssertNil(KeyMap.evdev(macKeyCode: kVK_Function))
    }

    func testEveryANSIKeyMapsToADistinctCode() {
        let ansi = [kVK_ANSI_A, kVK_ANSI_S, kVK_ANSI_D, kVK_ANSI_F, kVK_ANSI_H, kVK_ANSI_G, kVK_ANSI_Z, kVK_ANSI_X,
                    kVK_ANSI_C, kVK_ANSI_V, kVK_ANSI_B, kVK_ANSI_Q, kVK_ANSI_W, kVK_ANSI_E, kVK_ANSI_R, kVK_ANSI_Y,
                    kVK_ANSI_T, kVK_ANSI_1, kVK_ANSI_2, kVK_ANSI_3, kVK_ANSI_4, kVK_ANSI_6, kVK_ANSI_5, kVK_ANSI_Equal,
                    kVK_ANSI_9, kVK_ANSI_7, kVK_ANSI_Minus, kVK_ANSI_8, kVK_ANSI_0, kVK_ANSI_RightBracket, kVK_ANSI_O,
                    kVK_ANSI_U, kVK_ANSI_LeftBracket, kVK_ANSI_I, kVK_ANSI_P, kVK_ANSI_L, kVK_ANSI_J, kVK_ANSI_Quote,
                    kVK_ANSI_K, kVK_ANSI_Semicolon, kVK_ANSI_Backslash, kVK_ANSI_Comma, kVK_ANSI_Slash, kVK_ANSI_N,
                    kVK_ANSI_M, kVK_ANSI_Period, kVK_ANSI_Grave, kVK_Return, kVK_Tab, kVK_Space, kVK_Delete, kVK_Escape]
        let codes = ansi.compactMap { KeyMap.evdev(macKeyCode: $0) }
        XCTAssertEqual(codes.count, ansi.count)
        XCTAssertEqual(Set(codes).count, ansi.count)
    }

    func testLetterboxMapping() {
        // 1920x1080 in a 1000x1000 view: 1000x562.5, bars of 218.75 above
        // and below.
        let view = CGSize(width: 1000, height: 1000), guest = CGSize(width: 1920, height: 1080)
        let fit = GuestGeometry.fit(guest: guest, in: view)
        XCTAssertEqual(fit.minY, 218.75, accuracy: 0.001)
        XCTAssertEqual(fit.height, 562.5, accuracy: 0.001)
        // Centre.
        XCTAssertTrue(GuestGeometry.guestPoint(CGPoint(x: 500, y: 500), viewSize: view, guestSize: guest, flipped: false)! == (960, 540))
        // The image's top-left corner: y up (an unflipped NSView) vs down.
        XCTAssertTrue(GuestGeometry.guestPoint(CGPoint(x: 0, y: 781.25), viewSize: view, guestSize: guest, flipped: false)! == (0, 0))
        XCTAssertTrue(GuestGeometry.guestPoint(CGPoint(x: 0, y: 218.75), viewSize: view, guestSize: guest, flipped: true)! == (0, 0))
        // Bottom-right corner clamps to the last pixel.
        XCTAssertTrue(GuestGeometry.guestPoint(CGPoint(x: 1000, y: 218.75), viewSize: view, guestSize: guest, flipped: false)! == (1919, 1079))
        // The bars.
        XCTAssertNil(GuestGeometry.guestPoint(CGPoint(x: 500, y: 100), viewSize: view, guestSize: guest, flipped: false))
        XCTAssertNil(GuestGeometry.guestPoint(CGPoint(x: 500, y: 900), viewSize: view, guestSize: guest, flipped: false))
        XCTAssertNil(GuestGeometry.guestPoint(CGPoint(x: 500, y: 100), viewSize: view, guestSize: guest, flipped: true))
    }

    func testFlip() {
        let view = CGSize(width: 192, height: 108), guest = CGSize(width: 1920, height: 1080)
        // 10 points from the bottom of an unflipped view is near the guest's bottom.
        XCTAssertTrue(GuestGeometry.guestPoint(CGPoint(x: 0, y: 10), viewSize: view, guestSize: guest, flipped: false)! == (0, 980))
        XCTAssertTrue(GuestGeometry.guestPoint(CGPoint(x: 0, y: 10), viewSize: view, guestSize: guest, flipped: true)! == (0, 100))
    }

    func testScrollStepsOf15Points() {
        var scroll = ScrollAccumulator()
        // A trackpad's small deltas add up; Mac +y (towards the top) is
        // Linux's negative.
        XCTAssertTrue(scroll.add(dx: 0, dy: -10, precise: true) == (0, 0))
        XCTAssertTrue(scroll.add(dx: 0, dy: -10, precise: true) == (0, 1))
        XCTAssertTrue(scroll.add(dx: 0, dy: -10, precise: true) == (0, 1))
        XCTAssertTrue(scroll.add(dx: 0, dy: 45, precise: true) == (0, -3))
        XCTAssertTrue(scroll.add(dx: -15, dy: 0, precise: true) == (1, 0))
        // A mouse wheel's line is a click.
        var wheel = ScrollAccumulator()
        XCTAssertTrue(wheel.add(dx: 0, dy: -1, precise: false) == (0, 1))
        XCTAssertEqual(ScrollAccumulator.event((0, 1)), .scroll(dx: 0, dy: 15, discrete: true))
        XCTAssertNil(ScrollAccumulator.event((0, 0)))
    }

    func testModifiersToggle() {
        var tracker = ModifierTracker()
        XCTAssertEqual(tracker.flagsChanged(macKeyCode: kVK_Shift), [.key(code: 42, down: true)])
        XCTAssertEqual(tracker.flagsChanged(macKeyCode: kVK_RightShift), [.key(code: 54, down: true)])
        XCTAssertEqual(tracker.flagsChanged(macKeyCode: kVK_Shift), [.key(code: 42, down: false)])
        XCTAssertEqual(tracker.flagsChanged(macKeyCode: kVK_CapsLock), [.key(code: 58, down: true), .key(code: 58, down: false)])
        XCTAssertEqual(tracker.flagsChanged(macKeyCode: kVK_ANSI_A), [])
        XCTAssertEqual(tracker.held, [kVK_RightShift])
    }

    func testButtons() {
        XCTAssertEqual(GuestButton.evdev(0), 272)
        XCTAssertEqual(GuestButton.evdev(1), 273)
        XCTAssertEqual(GuestButton.evdev(2), 274)
        XCTAssertNil(GuestButton.evdev(5))
    }
}

/// The lines as docs/display-protocol.md spells them (the copy in
/// docs/plans/phase3-build.md, architecture section 3).
final class ProtocolStringTests: XCTestCase {
    func testInputLines() {
        let cases: [(InputEvent, String)] = [
            (.pointer(x: 12, y: 34), #"{"ev":"pointer","x":12,"y":34}"#),
            (.button(272, down: true), #"{"ev":"button","button":272,"down":true}"#),
            (.button(274, down: false), #"{"ev":"button","button":274,"down":false}"#),
            (.scroll(dx: 0, dy: -15, discrete: true), #"{"ev":"scroll","dx":0,"dy":-15,"discrete":true}"#),
            (.key(code: 30, down: true), #"{"ev":"key","code":30,"down":true}"#),
            (.releaseAll, #"{"ev":"release_all"}"#),
            (.control(on: true), #"{"ev":"control","on":true}"#),
            (.control(on: false), #"{"ev":"control","on":false}"#),
        ]
        for (event, line) in cases {
            XCTAssertEqual(event.json, line)
            XCTAssertNotNil(jsonObject(event.json), "valid JSON")
        }
    }

    func testDisplayRequests() throws {
        let session = SurfaceStream.request(machine: "dev", source: .session("s-x"), maxFPS: 60)
        XCTAssertEqual(session as NSDictionary, jsonObject(#"{"cmd":"display","machine":"dev","session":"s-x","max_fps":60}"#))
        let scanout = SurfaceStream.request(machine: "dev", source: .scanout(0), maxFPS: 120)
        XCTAssertEqual(scanout as NSDictionary, jsonObject(#"{"cmd":"display","machine":"dev","scanout":0,"max_fps":120}"#))
        // A desktop's scanout names its session too: input goes there.
        let desktop = SurfaceStream.request(machine: "dev", source: .scanout(0), maxFPS: 60, inputSession: "s-d")
        XCTAssertEqual(desktop as NSDictionary,
                       jsonObject(#"{"cmd":"display","machine":"dev","scanout":0,"session":"s-d","max_fps":60}"#))
        let target = DisplayTarget(target: "linux:dev", session: "s-d", scanout: 0)
        XCTAssertEqual(SurfaceStream(target: target).inputSession, "s-d")
        XCTAssertEqual(SurfaceStream(machine: "m", source: .scanout(0), maxFPS: 500).maxFPS, 120)
        XCTAssertEqual(SurfaceStream(machine: "m", source: .scanout(0), maxFPS: 0).maxFPS, 1)
    }

    func testReplyAndDoorbellsDecode() throws {
        let reply = try JSONDecoder().decode(RingInfo.self, from: Data(
            #"{"ok":true,"transports":["ring"],"ring":"/r/a.1.ring","gen":1,"width":1920,"height":1080,"stride":7680,"format":"bgra8","slots":3,"live":true}"#.utf8))
        XCTAssertEqual(reply.ring, "/r/a.1.ring")
        XCTAssertEqual(reply.stride, 7680)
        XCTAssertEqual(reply.transports, ["ring"])
        let frame = try JSONDecoder().decode(Doorbell.self, from: Data(#"{"ev":"frame","slot":1,"seq":42,"damage":[[0,0,10,10]]}"#.utf8))
        XCTAssertEqual(frame, Doorbell(ev: "frame", slot: 1, seq: 42, damage: [[0, 0, 10, 10]]))
        let end = try JSONDecoder().decode(Doorbell.self, from: Data(#"{"ev":"end","error":"session ended"}"#.utf8))
        XCTAssertEqual(end.error, "session ended")
    }
}
