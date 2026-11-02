// Synthetic input through CGEvent, posted at the HID level so it behaves like
// the VM's own mouse and keyboard (needs the Accessibility grant). One lock
// serialises input: two connections typing at once would interleave keys.
// Screen coordinates are points with the origin at the top-left of the main
// display, the same space AX and the protocol use.
import CoreGraphics
import Foundation
import GuiCore

enum Input {
    static let lock = NSLock()
    static let source = CGEventSource(stateID: .hidSystemState)

    static func post(_ e: CGEvent?) {
        e?.post(tap: .cghidEventTap)
    }

    /// A short pause between events: apps read the event queue on their own
    /// run loop, and events posted back to back can coalesce or reorder.
    static func pause(_ ms: Int = 15) { usleep(useconds_t(ms * 1000)) }

    static func flags(_ m: Modifiers) -> CGEventFlags {
        var f: CGEventFlags = []
        if m.contains(.cmd) { f.insert(.maskCommand) }
        if m.contains(.ctrl) { f.insert(.maskControl) }
        if m.contains(.alt) { f.insert(.maskAlternate) }
        if m.contains(.shift) { f.insert(.maskShift) }
        if m.contains(.fn) { f.insert(.maskSecondaryFn) }
        return f
    }

    static func move(to p: CGPoint) {
        post(CGEvent(mouseEventSource: source, mouseType: .mouseMoved, mouseCursorPosition: p, mouseButton: .left))
        pause()
    }

    static func click(at p: CGPoint, right: Bool, count: Int) {
        lock.lock(); defer { lock.unlock() }
        let (down, up, button): (CGEventType, CGEventType, CGMouseButton) =
            right ? (.rightMouseDown, .rightMouseUp, .right) : (.leftMouseDown, .leftMouseUp, .left)
        move(to: p)
        for n in 1...max(1, min(count, 3)) {
            // clickState tells the app this is the n-th click of a double or
            // triple click; without it two clicks are two single clicks.
            let d = CGEvent(mouseEventSource: source, mouseType: down, mouseCursorPosition: p, mouseButton: button)
            d?.setIntegerValueField(.mouseEventClickState, value: Int64(n))
            post(d)
            pause(30)
            let u = CGEvent(mouseEventSource: source, mouseType: up, mouseCursorPosition: p, mouseButton: button)
            u?.setIntegerValueField(.mouseEventClickState, value: Int64(n))
            post(u)
            pause(40)
        }
    }

    static func drag(from a: CGPoint, to b: CGPoint) {
        lock.lock(); defer { lock.unlock() }
        move(to: a)
        post(CGEvent(mouseEventSource: source, mouseType: .leftMouseDown, mouseCursorPosition: a, mouseButton: .left))
        pause(80)
        // Intermediate drag events: many apps only start a drag after the
        // pointer moves a few points with the button held.
        let steps = 20
        for i in 1...steps {
            let t = Double(i) / Double(steps)
            let p = CGPoint(x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t)
            post(CGEvent(mouseEventSource: source, mouseType: .leftMouseDragged, mouseCursorPosition: p, mouseButton: .left))
            pause(15)
        }
        pause(60)
        post(CGEvent(mouseEventSource: source, mouseType: .leftMouseUp, mouseCursorPosition: b, mouseButton: .left))
        pause()
    }

    /// dx/dy in points; positive dy scrolls down (content moves up), as in a
    /// browser's deltaY. CGEvent's wheel counts the other way.
    static func scroll(at p: CGPoint, dx: Double, dy: Double) {
        lock.lock(); defer { lock.unlock() }
        move(to: p)
        let e = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2,
                        wheel1: Int32(clamping: Int(-dy.rounded())), wheel2: Int32(clamping: Int(-dx.rounded())), wheel3: 0)
        e?.location = p
        post(e)
        pause()
    }

    static func key(_ combo: KeyCombo) {
        lock.lock(); defer { lock.unlock() }
        let f = flags(combo.modifiers)
        let d = CGEvent(keyboardEventSource: source, virtualKey: combo.keyCode, keyDown: true)
        d?.flags = f
        post(d)
        pause(20)
        let u = CGEvent(keyboardEventSource: source, virtualKey: combo.keyCode, keyDown: false)
        u?.flags = f
        post(u)
        pause()
    }

    /// Types text as Unicode, independent of the keyboard layout. Newlines and
    /// tabs go as the Return and Tab keys, which is what fields act on.
    static func type(_ text: String) {
        lock.lock(); defer { lock.unlock() }
        var run: [UniChar] = []
        func flush() {
            // CGEvent carries at most 20 UTF-16 units per event.
            var i = 0
            while i < run.count {
                var end = min(i + 20, run.count)
                // Don't split a surrogate pair across events.
                if end < run.count, UTF16.isLeadSurrogate(run[end - 1]) { end -= 1 }
                let chunk = Array(run[i..<end])
                for down in [true, false] {
                    let e = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: down)
                    chunk.withUnsafeBufferPointer { e?.keyboardSetUnicodeString(stringLength: $0.count, unicodeString: $0.baseAddress) }
                    post(e)
                }
                pause(10)
                i = end
            }
            run.removeAll()
        }
        for ch in text {
            let special: UInt16? = ch == "\n" || ch == "\r\n" || ch == "\r" ? 36 : ch == "\t" ? 48 : nil
            if let code = special {
                flush()
                for down in [true, false] {
                    post(CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down))
                }
                pause(10)
            } else {
                run.append(contentsOf: ch.utf16)
            }
        }
        flush()
    }
}
