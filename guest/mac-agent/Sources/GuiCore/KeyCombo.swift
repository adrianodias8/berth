// Key combos such as "cmd+shift+s" for the `key` verb, turned into a macOS
// virtual key code plus modifiers. The codes are the ANSI keyboard positions
// from Carbon's Events.h (kVK_*), which CGEvent takes; they name physical
// keys, so letters land right on US-style layouts, which the guest image uses.
import Foundation

public struct Modifiers: OptionSet, Equatable, Hashable {
    public let rawValue: Int
    public init(rawValue: Int) { self.rawValue = rawValue }
    public static let cmd = Modifiers(rawValue: 1 << 0)
    public static let ctrl = Modifiers(rawValue: 1 << 1)
    public static let alt = Modifiers(rawValue: 1 << 2)
    public static let shift = Modifiers(rawValue: 1 << 3)
    public static let fn = Modifiers(rawValue: 1 << 4)
}

public struct KeyCombo: Equatable {
    public var keyCode: UInt16
    public var modifiers: Modifiers

    public init(keyCode: UInt16, modifiers: Modifiers) {
        self.keyCode = keyCode
        self.modifiers = modifiers
    }

    static let modifierNames: [String: Modifiers] = [
        "cmd": .cmd, "command": .cmd, "super": .cmd, "meta": .cmd, "win": .cmd,
        "ctrl": .ctrl, "control": .ctrl,
        "alt": .alt, "option": .alt, "opt": .alt,
        "shift": .shift,
        "fn": .fn,
    ]

    public static let keyCodes: [String: UInt16] = {
        var m: [String: UInt16] = [
            "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9,
            "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17,
            "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "=": 24, "9": 25, "7": 26,
            "-": 27, "8": 28, "0": 29, "]": 30, "o": 31, "u": 32, "[": 33, "i": 34, "p": 35,
            "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42, ",": 43, "/": 44,
            "n": 45, "m": 46, ".": 47, "`": 50,
            "return": 36, "enter": 36, "tab": 48, "space": 49,
            // "delete" is forward delete on both guests (as X11 and the
            // web name it); the Mac's ⌫ key is "backspace".
            "backspace": 51, "escape": 53, "esc": 53,
            "delete": 117, "del": 117, "forwarddelete": 117, "home": 115, "end": 119, "pageup": 116, "pagedown": 121,
            "left": 123, "right": 124, "down": 125, "up": 126,
            "help": 114, "capslock": 57, "keypadenter": 76,
            "f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97, "f7": 98,
            "f8": 100, "f9": 101, "f10": 109, "f11": 103, "f12": 111,
            "f13": 105, "f14": 107, "f15": 113, "f16": 106, "f17": 64, "f18": 79,
            "f19": 80, "f20": 90,
            // A modifier pressed on its own ("shift").
            "cmd": 55, "command": 55, "shift": 56, "alt": 58, "option": 58,
            "ctrl": 59, "control": 59,
        ]
        // Spelled-out aliases, for combos where the character itself is awkward.
        let aliases: [String: String] = [
            "minus": "-", "equal": "=", "equals": "=", "comma": ",", "period": ".", "dot": ".",
            "slash": "/", "backslash": "\\", "semicolon": ";", "quote": "'", "grave": "`",
            "backtick": "`", "leftbracket": "[", "rightbracket": "]",
            "arrowleft": "left", "arrowright": "right", "arrowup": "up", "arrowdown": "down",
            "del": "forwarddelete", "pgup": "pageup", "pgdn": "pagedown", "ret": "return",
        ]
        for (alias, key) in aliases { m[alias] = m[key] }
        return m
    }()

    /// Parses "cmd+shift+a". Names are case-insensitive; the last part is the
    /// key and everything before it a modifier. "plus" is shift+= (US layout),
    /// and a trailing "+" (as in "cmd++") means the same.
    public static func parse(_ combo: String) throws -> KeyCombo {
        var text = combo.trimmingCharacters(in: .whitespaces).lowercased()
        guard !text.isEmpty else { throw GuiError("empty key combo") }
        var extra: Modifiers = []
        if text == "+" || text.hasSuffix("++") {
            text = String(text.dropLast()) + "plus"
        }
        var parts = text.split(separator: "+", omittingEmptySubsequences: false).map {
            $0.trimmingCharacters(in: .whitespaces)
        }
        guard let last = parts.popLast(), !last.isEmpty else {
            throw GuiError("key combo '\(combo)' has no key")
        }
        var mods: Modifiers = []
        for p in parts {
            guard let m = modifierNames[p] else {
                throw GuiError("unknown modifier '\(p)' in '\(combo)' (use cmd, ctrl, alt, shift)")
            }
            mods.insert(m)
        }
        var key = last
        if key == "plus" {
            key = "="
            extra.insert(.shift)
        }
        guard let code = keyCodes[key] else {
            throw GuiError("unknown key '\(last)' in '\(combo)'")
        }
        return KeyCombo(keyCode: code, modifiers: mods.union(extra))
    }
}
