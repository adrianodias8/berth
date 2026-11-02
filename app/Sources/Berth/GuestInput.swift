import AppKit
import Carbon.HIToolbox

// "Take control" of a guest's screen: Mac events turned into the display
// stream's input lines (docs/display-protocol.md), which the daemon hands
// to the session's virtual pointer and keyboard while control is on.
// Coordinates are the guest's pixels; keys are Linux evdev codes, and the
// guest's US keymap turns them into characters.

/// One input line. `json` is the exact line sent, `ev` first.
enum InputEvent: Equatable {
    case pointer(x: Int, y: Int)
    /// evdev BTN_LEFT 272, BTN_RIGHT 273, BTN_MIDDLE 274.
    case button(Int, down: Bool)
    /// In points, positive `dy` scrolling the content down (as the agent's
    /// `scroll` verb); `discrete` says these are wheel clicks of 15 points.
    case scroll(dx: Int, dy: Int, discrete: Bool)
    case key(code: Int, down: Bool)
    case releaseAll
    case control(on: Bool)

    var json: String {
        switch self {
        case let .pointer(x, y): return #"{"ev":"pointer","x":\#(x),"y":\#(y)}"#
        case let .button(button, down): return #"{"ev":"button","button":\#(button),"down":\#(down)}"#
        case let .scroll(dx, dy, discrete): return #"{"ev":"scroll","dx":\#(dx),"dy":\#(dy),"discrete":\#(discrete)}"#
        case let .key(code, down): return #"{"ev":"key","code":\#(code),"down":\#(down)}"#
        case .releaseAll: return #"{"ev":"release_all"}"#
        case let .control(on): return #"{"ev":"control","on":\#(on)}"#
        }
    }
}

enum GuestButton {
    static let left = 272, right = 273, middle = 274

    /// NSEvent's buttonNumber: 0 left, 1 right, 2 middle; others unmapped.
    static func evdev(_ buttonNumber: Int) -> Int? {
        switch buttonNumber {
        case 0: return left
        case 1: return right
        case 2: return middle
        default: return nil
        }
    }
}

enum KeyMap {
    /// The evdev code for a Mac virtual key code (Carbon's kVK_*), or nil
    /// for keys with no Linux counterpart (Fn).
    static func evdev(macKeyCode: Int) -> Int? { table[macKeyCode] }

    /// Modifier keys, which come as flagsChanged rather than keyDown/keyUp.
    static let modifiers: Set<Int> = [
        kVK_Shift, kVK_RightShift, kVK_Control, kVK_RightControl,
        kVK_Option, kVK_RightOption, kVK_Command, kVK_RightCommand, kVK_CapsLock,
    ]

    private static let table: [Int: Int] = [
        // Letters.
        kVK_ANSI_A: 30, kVK_ANSI_B: 48, kVK_ANSI_C: 46, kVK_ANSI_D: 32, kVK_ANSI_E: 18,
        kVK_ANSI_F: 33, kVK_ANSI_G: 34, kVK_ANSI_H: 35, kVK_ANSI_I: 23, kVK_ANSI_J: 36,
        kVK_ANSI_K: 37, kVK_ANSI_L: 38, kVK_ANSI_M: 50, kVK_ANSI_N: 49, kVK_ANSI_O: 24,
        kVK_ANSI_P: 25, kVK_ANSI_Q: 16, kVK_ANSI_R: 19, kVK_ANSI_S: 31, kVK_ANSI_T: 20,
        kVK_ANSI_U: 22, kVK_ANSI_V: 47, kVK_ANSI_W: 17, kVK_ANSI_X: 45, kVK_ANSI_Y: 21,
        kVK_ANSI_Z: 44,
        // The number row.
        kVK_ANSI_1: 2, kVK_ANSI_2: 3, kVK_ANSI_3: 4, kVK_ANSI_4: 5, kVK_ANSI_5: 6,
        kVK_ANSI_6: 7, kVK_ANSI_7: 8, kVK_ANSI_8: 9, kVK_ANSI_9: 10, kVK_ANSI_0: 11,
        kVK_ANSI_Minus: 12, kVK_ANSI_Equal: 13,
        // Punctuation.
        kVK_ANSI_LeftBracket: 26, kVK_ANSI_RightBracket: 27, kVK_ANSI_Semicolon: 39,
        kVK_ANSI_Quote: 40, kVK_ANSI_Grave: 41, kVK_ANSI_Backslash: 43, kVK_ANSI_Comma: 51,
        kVK_ANSI_Period: 52, kVK_ANSI_Slash: 53, kVK_ISO_Section: 86,
        // Editing and whitespace.
        kVK_Escape: 1, kVK_Delete: 14, kVK_Tab: 15, kVK_Return: 28, kVK_Space: 57,
        kVK_ForwardDelete: 111, kVK_Help: 110, kVK_Home: 102, kVK_End: 107,
        kVK_PageUp: 104, kVK_PageDown: 109,
        // Arrows.
        kVK_UpArrow: 103, kVK_LeftArrow: 105, kVK_RightArrow: 106, kVK_DownArrow: 108,
        // Modifiers: the Command keys are Super (Meta) in the guest.
        kVK_Control: 29, kVK_Shift: 42, kVK_RightShift: 54, kVK_Option: 56,
        kVK_CapsLock: 58, kVK_RightControl: 97, kVK_RightOption: 100,
        kVK_Command: 125, kVK_RightCommand: 126,
        // Function keys.
        kVK_F1: 59, kVK_F2: 60, kVK_F3: 61, kVK_F4: 62, kVK_F5: 63, kVK_F6: 64,
        kVK_F7: 65, kVK_F8: 66, kVK_F9: 67, kVK_F10: 68, kVK_F11: 87, kVK_F12: 88,
        kVK_F13: 183, kVK_F14: 184, kVK_F15: 185, kVK_F16: 186, kVK_F17: 187,
        kVK_F18: 188, kVK_F19: 189, kVK_F20: 190,
        // Keypad. Clear sits where Num Lock is on a PC keypad.
        kVK_ANSI_Keypad0: 82, kVK_ANSI_Keypad1: 79, kVK_ANSI_Keypad2: 80, kVK_ANSI_Keypad3: 81,
        kVK_ANSI_Keypad4: 75, kVK_ANSI_Keypad5: 76, kVK_ANSI_Keypad6: 77, kVK_ANSI_Keypad7: 71,
        kVK_ANSI_Keypad8: 72, kVK_ANSI_Keypad9: 73, kVK_ANSI_KeypadDecimal: 83,
        kVK_ANSI_KeypadMultiply: 55, kVK_ANSI_KeypadPlus: 78, kVK_ANSI_KeypadMinus: 74,
        kVK_ANSI_KeypadDivide: 98, kVK_ANSI_KeypadEnter: 96, kVK_ANSI_KeypadEquals: 117,
        kVK_ANSI_KeypadClear: 69,
        // Media keys that arrive as key codes.
        kVK_Mute: 113, kVK_VolumeDown: 114, kVK_VolumeUp: 115,
        // JIS keyboards.
        kVK_JIS_Yen: 124, kVK_JIS_Underscore: 89, kVK_JIS_KeypadComma: 121,
        kVK_JIS_Eisu: 94, kVK_JIS_Kana: 93,
    ]
}

enum GuestGeometry {
    /// Where a `guest`-sized image sits, aspect-fit and centred, in a
    /// `view`-sized area (y up or down doesn't matter: it's symmetric).
    static func fit(guest: CGSize, in view: CGSize) -> CGRect {
        guard guest.width > 0, guest.height > 0, view.width > 0, view.height > 0 else { return .zero }
        let scale = min(view.width / guest.width, view.height / guest.height)
        let size = CGSize(width: guest.width * scale, height: guest.height * scale)
        return CGRect(x: (view.width - size.width) / 2, y: (view.height - size.height) / 2,
                      width: size.width, height: size.height)
    }

    /// The guest pixel under `point` (view coordinates; `flipped` when y
    /// grows downwards, as in a flipped NSView), or nil in the letterbox.
    static func guestPoint(_ point: CGPoint, viewSize: CGSize, guestSize: CGSize, flipped: Bool) -> (x: Int, y: Int)? {
        let rect = fit(guest: guestSize, in: viewSize)
        guard rect.width > 0 else { return nil }
        let fromTop = flipped ? point.y : viewSize.height - point.y
        let x = point.x - rect.minX, y = fromTop - rect.minY
        guard x >= 0, y >= 0, x <= rect.width, y <= rect.height else { return nil }
        let gx = Int(x / rect.width * guestSize.width), gy = Int(y / rect.height * guestSize.height)
        return (min(gx, Int(guestSize.width) - 1), min(gy, Int(guestSize.height) - 1))
    }
}

/// Turns the Mac's scroll deltas into wheel clicks: a trackpad's precise
/// deltas add up, 15 points a click (what libinput reports for a wheel);
/// a mouse wheel's lines count as 15 points each. Signs flip to Linux's
/// (positive scrolls the content down), whatever natural scrolling says
/// on the Mac, the deltas already include it.
struct ScrollAccumulator {
    static let step: CGFloat = 15
    private var x: CGFloat = 0, y: CGFloat = 0

    /// Whole clicks along each axis, the remainder kept for next time.
    mutating func add(dx: CGFloat, dy: CGFloat, precise: Bool) -> (dx: Int, dy: Int) {
        let unit: CGFloat = precise ? 1 : Self.step
        x -= dx * unit
        y -= dy * unit
        let cx = Int((x / Self.step).rounded(.towardZero)), cy = Int((y / Self.step).rounded(.towardZero))
        x -= CGFloat(cx) * Self.step
        y -= CGFloat(cy) * Self.step
        return (cx, cy)
    }

    mutating func reset() { x = 0; y = 0 }

    /// The input line for the clicks `add` returned, nil when there are none.
    static func event(_ clicks: (dx: Int, dy: Int)) -> InputEvent? {
        guard clicks.dx != 0 || clicks.dy != 0 else { return nil }
        return .scroll(dx: clicks.dx * Int(step), dy: clicks.dy * Int(step), discrete: true)
    }
}

/// Which modifier keys are down, from flagsChanged's key codes: each one
/// toggles its key (Caps Lock taps, since the Mac reports its lock state).
struct ModifierTracker {
    private(set) var held: Set<Int> = []

    mutating func flagsChanged(macKeyCode: Int) -> [InputEvent] {
        guard KeyMap.modifiers.contains(macKeyCode), let code = KeyMap.evdev(macKeyCode: macKeyCode) else { return [] }
        if macKeyCode == kVK_CapsLock { return [.key(code: code, down: true), .key(code: code, down: false)] }
        if held.remove(macKeyCode) != nil { return [.key(code: code, down: false)] }
        held.insert(macKeyCode)
        return [.key(code: code, down: true)]
    }

    mutating func reset() { held.removeAll() }
}
