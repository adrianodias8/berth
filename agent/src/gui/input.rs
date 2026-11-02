//! `berth-agent gui-input <w>x<h>`, inside a session: input lines on stdin
//! become a virtual pointer and keyboard on the session's compositor, for
//! the `input.stream` verb (docs/gui-protocol.md, Streams; the lines are
//! docs/display-protocol.md's).
//!
//! Unlike gui-pointer and wtype, which make a device per command, it keeps
//! one zwlr_virtual_pointer_v1 and one zwp_virtual_keyboard_v1 for as long
//! as the stream lasts, so a key held on the Mac stays held and toolkits see
//! one keyboard (and one keymap) throughout. The keymap is US
//! (keymap-us.xkb, compiled by the compositor against the machine's
//! xkeyboard-config as wtype's is); keys are evdev codes. The modifier mask
//! follows the modifier keys held. EOF, or `control` off, releases
//! everything still held.

use std::io::{self, BufRead, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::capture::error_line;
use super::wayland::{self, Arg, Conn, encode};

pub const KEYMAP: &str = include_str!("keymap-us.xkb");
/// zwp_virtual_keyboard_v1.keymap's format: XKB_KEYMAP_FORMAT_TEXT_V1.
const KEYMAP_XKB_V1: u32 = 1;

/// xkb modifier masks, in the order every xkeyboard-config keymap has them.
const SHIFT: u32 = 1;
const LOCK: u32 = 1 << 1;
const CONTROL: u32 = 1 << 2;
const MOD1: u32 = 1 << 3; // Alt
const MOD2: u32 = 1 << 4; // Num Lock
const MOD4: u32 = 1 << 6; // Super

const KEY_CAPSLOCK: u32 = 58;
const KEY_NUMLOCK: u32 = 69;
/// Evdev codes run below KEY_MAX (0x2ff).
const KEY_LIMIT: u32 = 0x300;
/// BTN_LEFT to BTN_TASK.
const BUTTONS: std::ops::RangeInclusive<u32> = 0x110..=0x117;

/// wl_pointer.axis_source.
const SOURCE_WHEEL: u32 = 0;
const SOURCE_CONTINUOUS: u32 = 2;

/// One input line.
#[derive(Deserialize, Debug, PartialEq)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Ev {
    /// Absolute, in screen points.
    Pointer { x: f64, y: f64 },
    Button { button: u32, down: bool },
    /// Points; positive `dy` scrolls the content down. `discrete` (true, or
    /// a non-zero count) sends it as mouse-wheel clicks of 15 points.
    Scroll {
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
        #[serde(default)]
        discrete: Value,
    },
    Key { code: u32, down: bool },
    ReleaseAll,
    Control { on: bool },
}

fn modifier(code: u32) -> u32 {
    match code {
        42 | 54 => SHIFT,
        29 | 97 => CONTROL,
        56 | 100 => MOD1,
        125 | 126 => MOD4,
        _ => 0,
    }
}

/// The two devices and what is held on them. Each line becomes the bytes of
/// its requests, which the caller sends.
pub struct Devices {
    pointer: u32,
    keyboard: u32,
    extent: [u32; 2],
    keys: Vec<u32>,
    buttons: Vec<u32>,
    locked: u32,
    /// The (depressed, locked) masks the compositor has.
    sent: (u32, u32),
}

impl Devices {
    pub fn new(pointer: u32, keyboard: u32, extent: [u32; 2]) -> Devices {
        Devices { pointer, keyboard, extent, keys: Vec::new(), buttons: Vec::new(), locked: 0, sent: (0, 0) }
    }

    /// The requests for one input line at time `t` (ms).
    pub fn line(&mut self, line: &str, t: u32) -> Result<Vec<u8>, String> {
        let ev: Ev = serde_json::from_str(line).map_err(|e| format!("bad input line: {e}"))?;
        self.apply(ev, t)
    }

    pub fn apply(&mut self, ev: Ev, t: u32) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        match ev {
            Ev::Pointer { x, y } => {
                if !x.is_finite() || !y.is_finite() {
                    return Err("pointer: x and y are numbers".into());
                }
                let [w, h] = self.extent;
                let x = x.round().clamp(0.0, f64::from(w.saturating_sub(1))) as u32;
                let y = y.round().clamp(0.0, f64::from(h.saturating_sub(1))) as u32;
                out.extend(encode(self.pointer, 1, &[Arg::U(t), Arg::U(x), Arg::U(y), Arg::U(w), Arg::U(h)]));
                out.extend(self.frame());
            }
            Ev::Button { button, down } => {
                if !BUTTONS.contains(&button) {
                    return Err(format!("button {button:#x}: BTN_LEFT (0x110) to BTN_TASK (0x117)"));
                }
                // A press already held (or a release of nothing) changes nothing.
                if down == self.buttons.contains(&button) {
                    return Ok(out);
                }
                if down {
                    self.buttons.push(button);
                } else {
                    self.buttons.retain(|&b| b != button);
                }
                out.extend(encode(self.pointer, 2, &[Arg::U(t), Arg::U(button), Arg::U(u32::from(down))]));
                out.extend(self.frame());
            }
            Ev::Scroll { dx, dy, discrete } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return Err("scroll: dx and dy are numbers".into());
                }
                if dx == 0.0 && dy == 0.0 {
                    return Ok(out);
                }
                let wheel = match discrete {
                    Value::Bool(b) => b,
                    Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
                    _ => false,
                };
                out.extend(encode(self.pointer, 5, &[Arg::U(if wheel { SOURCE_WHEEL } else { SOURCE_CONTINUOUS })]));
                for (axis, value) in [(0u32, dy), (1, dx)] {
                    if value == 0.0 {
                        continue;
                    }
                    if wheel {
                        out.extend(encode(self.pointer, 7, &[Arg::U(t), Arg::U(axis), Arg::F(value), Arg::I(wayland::wheel_clicks(value))]));
                    } else {
                        out.extend(encode(self.pointer, 3, &[Arg::U(t), Arg::U(axis), Arg::F(value)]));
                    }
                }
                out.extend(self.frame());
            }
            Ev::Key { code, down } => {
                if code >= KEY_LIMIT {
                    return Err(format!("key {code:#x}: evdev codes are below 0x300"));
                }
                if down == self.keys.contains(&code) {
                    return Ok(out);
                }
                if down {
                    self.keys.push(code);
                    match code {
                        KEY_CAPSLOCK => self.locked ^= LOCK,
                        KEY_NUMLOCK => self.locked ^= MOD2,
                        _ => {}
                    }
                } else {
                    self.keys.retain(|&k| k != code);
                }
                out.extend(encode(self.keyboard, 1, &[Arg::U(t), Arg::U(code), Arg::U(u32::from(down))]));
                out.extend(self.modifiers());
            }
            Ev::ReleaseAll => out = self.release_all(t),
            Ev::Control { on } => {
                if !on {
                    out = self.release_all(t);
                }
            }
        }
        Ok(out)
    }

    /// Releases for every key and button held.
    pub fn release_all(&mut self, t: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for code in std::mem::take(&mut self.keys) {
            out.extend(encode(self.keyboard, 1, &[Arg::U(t), Arg::U(code), Arg::U(0)]));
        }
        out.extend(self.modifiers());
        let buttons = std::mem::take(&mut self.buttons);
        for &button in &buttons {
            out.extend(encode(self.pointer, 2, &[Arg::U(t), Arg::U(button), Arg::U(0)]));
        }
        if !buttons.is_empty() {
            out.extend(self.frame());
        }
        out
    }

    fn frame(&self) -> Vec<u8> {
        encode(self.pointer, 4, &[])
    }

    /// zwp_virtual_keyboard_v1.modifiers, when the mask has changed.
    fn modifiers(&mut self) -> Vec<u8> {
        let depressed = self.keys.iter().fold(0, |m, &k| m | modifier(k));
        if (depressed, self.locked) == self.sent {
            return Vec::new();
        }
        self.sent = (depressed, self.locked);
        encode(self.keyboard, 2, &[Arg::U(depressed), Arg::U(0), Arg::U(self.locked), Arg::U(0)])
    }
}

/// The keymap as the compositor reads it: NUL-terminated.
pub fn keymap_contents() -> Vec<u8> {
    let mut map = KEYMAP.as_bytes().to_vec();
    map.push(0);
    map
}

/// zwp_virtual_keyboard_v1.keymap(format, fd, size): the fd goes beside it.
pub fn keymap_message(keyboard: u32, size: usize) -> Vec<u8> {
    encode(keyboard, 0, &[Arg::U(KEYMAP_XKB_V1), Arg::Fd, Arg::U(size as u32)])
}

/// Feed `input`'s lines to the session until EOF. A bad line is answered
/// with an `{"error"}` line on stdout and skipped.
pub fn run(input: impl BufRead, extent: [u32; 2]) -> io::Result<()> {
    let mut conn = Conn::connect()?;
    let bound = conn.bind_globals(&[("wl_seat", 1), ("zwlr_virtual_pointer_manager_v1", 1), ("zwp_virtual_keyboard_manager_v1", 1)])?;
    let seat = bound[0].ok_or_else(|| io::Error::other("the compositor has no seat"))?;
    let pointers = bound[1].ok_or_else(|| io::Error::other("the compositor has no virtual pointer (zwlr_virtual_pointer_manager_v1)"))?;
    let keyboards = bound[2].ok_or_else(|| io::Error::other("the compositor has no virtual keyboard (zwp_virtual_keyboard_manager_v1)"))?;
    // The pointer on the default seat; the keyboard needs one named.
    let pointer = conn.alloc();
    conn.send(pointers.id, 0, &[Arg::U(0), Arg::U(pointer)])?;
    let keyboard = conn.alloc();
    conn.send(keyboards.id, 0, &[Arg::U(seat.id), Arg::U(keyboard)])?;
    let map = keymap_contents();
    let mut file = std::fs::File::from(wayland::memfd("berth-keymap", map.len())?);
    file.write_all(&map)?;
    conn.send_with_fd(&keymap_message(keyboard, map.len()), file.as_raw_fd())?;
    conn.roundtrip(|_| {})?;
    let mut devices = Devices::new(pointer, keyboard, extent);
    let mut stdout = io::stdout();
    for line in input.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match devices.line(&line, wayland::now_ms()) {
            Ok(bytes) => conn.send_raw(&bytes)?,
            Err(err) => {
                let _ = stdout.write_all(error_line(&err).as_bytes()).and_then(|()| stdout.flush());
            }
        }
        conn.drain()?;
    }
    conn.send_raw(&devices.release_all(wayland::now_ms()))?;
    // Let toolkits take the releases before the devices go.
    conn.roundtrip(|_| {})?;
    std::thread::sleep(Duration::from_millis(200));
    conn.send(pointer, 8, &[])?;
    conn.send(keyboard, 3, &[])?;
    conn.roundtrip(|_| {})
}

/// `berth-agent gui-input <w>x<h>`.
pub fn local(args: &[String]) -> i32 {
    let extent = args.first().and_then(|a| a.split_once('x')).and_then(|(w, h)| Some([w.parse::<u32>().ok()?, h.parse::<u32>().ok()?]));
    let Some(extent) = extent.filter(|[w, h]| *w > 0 && *h > 0) else {
        eprintln!("usage: berth-agent gui-input <width>x<height>   (input lines on stdin)");
        return 2;
    };
    match run(io::stdin().lock(), extent) {
        Ok(()) => 0,
        Err(err) => {
            let mut out = io::stdout();
            let _ = out.write_all(error_line(&err.to_string()).as_bytes()).and_then(|()| out.flush());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::wayland::words;

    const P: u32 = 5;
    const K: u32 = 6;
    const T: u32 = 1000;

    fn frame() -> Vec<u8> {
        words(&[P, 8 << 16 | 4])
    }

    fn key(code: u32, down: bool) -> Vec<u8> {
        words(&[K, 20 << 16 | 1, T, code, u32::from(down)])
    }

    fn mods(depressed: u32, locked: u32) -> Vec<u8> {
        words(&[K, 24 << 16 | 2, depressed, 0, locked, 0])
    }

    #[test]
    fn pointer_lines() {
        let mut d = Devices::new(P, K, [1280, 800]);
        // motion_absolute(time, x, y, extent), clamped to the screen.
        assert_eq!(d.line(r#"{"ev":"pointer","x":10.4,"y":2000}"#, T).unwrap(), [words(&[P, 28 << 16 | 1, T, 10, 799, 1280, 800]), frame()].concat());
        assert_eq!(d.line(r#"{"ev":"button","button":272,"down":true}"#, T).unwrap(), [words(&[P, 20 << 16 | 2, T, 272, 1]), frame()].concat());
        assert!(d.line(r#"{"ev":"button","button":272,"down":true}"#, T).unwrap().is_empty(), "already held");
        // Wheel: axis_source(wheel), axis_discrete(time, vertical, 30.0, 2 clicks).
        assert_eq!(
            d.line(r#"{"ev":"scroll","dx":0,"dy":30,"discrete":true}"#, T).unwrap(),
            [words(&[P, 12 << 16 | 5, 0]), words(&[P, 24 << 16 | 7, T, 0, 30 * 256, 2]), frame()].concat()
        );
        // Precise: axis_source(continuous), axis(time, horizontal, -4.5).
        assert_eq!(
            d.line(r#"{"ev":"scroll","dx":-4.5,"dy":0}"#, T).unwrap(),
            [words(&[P, 12 << 16 | 5, 2]), words(&[P, 20 << 16 | 3, T, 1, (-4.5f64 * 256.0) as i32 as u32]), frame()].concat()
        );
    }

    #[test]
    fn modifiers_follow_the_keys_held() {
        let mut d = Devices::new(P, K, [1280, 800]);
        assert_eq!(d.line(r#"{"ev":"key","code":42,"down":true}"#, T).unwrap(), [key(42, true), mods(SHIFT, 0)].concat());
        assert_eq!(d.line(r#"{"ev":"key","code":29,"down":true}"#, T).unwrap(), [key(29, true), mods(SHIFT | CONTROL, 0)].concat());
        assert_eq!(d.line(r#"{"ev":"key","code":30,"down":true}"#, T).unwrap(), key(30, true), "a plain key leaves the mask");
        // Both shifts down, one up: still shifted.
        d.line(r#"{"ev":"key","code":54,"down":true}"#, T).unwrap();
        assert_eq!(d.line(r#"{"ev":"key","code":42,"down":false}"#, T).unwrap(), key(42, false));
        assert_eq!(d.line(r#"{"ev":"key","code":54,"down":false}"#, T).unwrap(), [key(54, false), mods(CONTROL, 0)].concat());
        // Caps Lock locks on its press.
        assert_eq!(d.line(r#"{"ev":"key","code":58,"down":true}"#, T).unwrap(), [key(58, true), mods(CONTROL, LOCK)].concat());
        d.line(r#"{"ev":"key","code":58,"down":false}"#, T).unwrap();
        assert_eq!(modifier(125) | modifier(100), MOD4 | MOD1);
    }

    #[test]
    fn eof_releases_what_is_held() {
        let mut d = Devices::new(P, K, [1280, 800]);
        d.line(r#"{"ev":"key","code":29,"down":true}"#, T).unwrap();
        d.line(r#"{"ev":"key","code":30,"down":true}"#, T).unwrap();
        d.line(r#"{"ev":"button","button":273,"down":true}"#, T).unwrap();
        let want = [key(29, false), key(30, false), mods(0, 0), words(&[P, 20 << 16 | 2, T, 273, 0]), frame()].concat();
        assert_eq!(d.release_all(T), want);
        assert!(d.release_all(T).is_empty(), "nothing left");
        // release_all and control off as lines do the same.
        d.line(r#"{"ev":"key","code":30,"down":true}"#, T).unwrap();
        assert_eq!(d.line(r#"{"ev":"release_all"}"#, T).unwrap(), key(30, false));
        d.line(r#"{"ev":"key","code":30,"down":true}"#, T).unwrap();
        assert!(d.line(r#"{"ev":"control","on":true}"#, T).unwrap().is_empty());
        assert_eq!(d.line(r#"{"ev":"control","on":false}"#, T).unwrap(), key(30, false));
    }

    #[test]
    fn bad_lines_are_rejected() {
        let mut d = Devices::new(P, K, [1280, 800]);
        assert!(d.line(r#"{"ev":"teleport","x":1}"#, T).unwrap_err().contains("unknown variant"));
        assert!(d.line(r#"{"ev":"key","code":768,"down":true}"#, T).is_err());
        assert!(d.line(r#"{"ev":"button","button":30,"down":true}"#, T).is_err());
        assert!(d.line(r#"{"ev":"pointer","x":"1"}"#, T).is_err());
        assert!(d.line("not json", T).is_err());
        assert!(d.keys.is_empty() && d.buttons.is_empty());
    }

    #[test]
    fn the_keymap_is_sent_nul_terminated() {
        let map = keymap_contents();
        assert_eq!(map.last(), Some(&0));
        assert_eq!(map.iter().filter(|&&b| b == 0).count(), 1);
        let text = std::str::from_utf8(&map[..map.len() - 1]).unwrap();
        for section in ["xkb_keycodes", "xkb_types", "xkb_compat", "xkb_symbols"] {
            assert_eq!(text.matches(section).count(), 1, "{section}");
        }
        assert!(text.contains("us"));
        // keymap(format 1, fd beside, size with the NUL).
        assert_eq!(keymap_message(K, map.len()), words(&[K, 16 << 16, 1, map.len() as u32]));
    }
}
