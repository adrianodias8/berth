//! A tiny Wayland client for pointer input: `berth-agent gui-pointer <plan>`,
//! run inside a session (as the machine's user, with its WAYLAND_DISPLAY).
//!
//! It speaks the Wayland wire format directly (no libwayland in a static
//! binary) and uses wlroots' virtual pointer protocol
//! (zwlr_virtual_pointer_manager_v1, which sway and cage offer), because the
//! distro tools that exist (wlrctl) move the pointer only relatively, and
//! ydotool needs /dev/uinput, which machines don't have. Absolute motion is
//! given in screen points against the screen's size (the extent).
//!
//! Wire format: each message is the object id (u32), then the size in bytes
//! (upper 16 bits) and opcode (lower 16), then the arguments: 32-bit ints,
//! 24.8 fixed point, and strings as a length (with the NUL) plus bytes padded
//! to 4. Everything is native-endian.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct Plan {
    pub extent: [u32; 2],
    pub steps: Vec<Step>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Move([i64; 2]),
    Button { button: u32, pressed: bool },
    /// Scroll by this many points; positive `dy` scrolls the content down.
    Axis { dx: f64, dy: f64 },
    Sleep(u64),
}

/// Linux input button codes.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

const DISPLAY: u32 = 1;
const LINGER_MS: u64 = 400;

enum Arg<'a> {
    U(u32),
    F(f64),
    S(&'a str),
}

fn encode(object: u32, opcode: u16, args: &[Arg]) -> Vec<u8> {
    let mut body = Vec::new();
    for arg in args {
        match arg {
            Arg::U(v) => body.extend_from_slice(&v.to_ne_bytes()),
            Arg::F(v) => body.extend_from_slice(&((v * 256.0).round() as i32).to_ne_bytes()),
            Arg::S(s) => {
                body.extend_from_slice(&(s.len() as u32 + 1).to_ne_bytes());
                body.extend_from_slice(s.as_bytes());
                body.push(0);
                while body.len() % 4 != 0 {
                    body.push(0);
                }
            }
        }
    }
    let mut msg = Vec::with_capacity(8 + body.len());
    msg.extend_from_slice(&object.to_ne_bytes());
    msg.extend_from_slice(&((((8 + body.len()) as u32) << 16) | u32::from(opcode)).to_ne_bytes());
    msg.extend_from_slice(&body);
    msg
}

struct Event {
    object: u32,
    opcode: u16,
    body: Vec<u8>,
}

fn u32_at(body: &[u8], at: usize) -> Option<u32> {
    body.get(at..at + 4).map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
}

/// A string argument at `at`: the string and where the next argument starts.
fn string_at(body: &[u8], at: usize) -> Option<(String, usize)> {
    let len = u32_at(body, at)? as usize;
    let bytes = body.get(at + 4..at + 4 + len)?;
    let s = String::from_utf8_lossy(bytes.strip_suffix(&[0]).unwrap_or(bytes)).into_owned();
    Some((s, at + 4 + len.div_ceil(4) * 4))
}

struct Conn {
    stream: UnixStream,
    next_id: u32,
}

impl Conn {
    /// A new object id: the server only accepts the next one in sequence.
    fn alloc(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn send(&mut self, object: u32, opcode: u16, args: &[Arg]) -> io::Result<()> {
        self.stream.write_all(&encode(object, opcode, args))
    }

    fn read_event(&mut self) -> io::Result<Event> {
        let mut header = [0u8; 8];
        self.stream.read_exact(&mut header)?;
        let object = u32::from_ne_bytes(header[0..4].try_into().unwrap());
        let word = u32::from_ne_bytes(header[4..8].try_into().unwrap());
        let size = (word >> 16) as usize;
        if size < 8 {
            return Err(io::Error::other("bad Wayland message"));
        }
        let mut body = vec![0u8; size - 8];
        self.stream.read_exact(&mut body)?;
        Ok(Event { object, opcode: (word & 0xffff) as u16, body })
    }

    /// wl_display.sync and wait for its callback, so every request so far
    /// has been handled; `on_event` sees the events before it.
    fn roundtrip(&mut self, mut on_event: impl FnMut(&Event)) -> io::Result<()> {
        let callback = self.alloc();
        self.send(DISPLAY, 0, &[Arg::U(callback)])?;
        loop {
            let event = self.read_event()?;
            if event.object == DISPLAY && event.opcode == 0 {
                // wl_display.error: object, code, message.
                let message = string_at(&event.body, 8).map(|(s, _)| s).unwrap_or_default();
                return Err(io::Error::other(format!("Wayland error: {message}")));
            }
            if event.object == callback && event.opcode == 0 {
                return Ok(());
            }
            on_event(&event);
        }
    }
}

fn now_ms() -> u32 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}

/// Carry out `plan` on the session's compositor.
pub fn run(plan: &Plan) -> io::Result<()> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("no XDG_RUNTIME_DIR"))?;
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let path = if display.starts_with('/') { display } else { format!("{runtime}/{display}") };
    let stream = UnixStream::connect(&path).map_err(|e| io::Error::other(format!("{path}: {e}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut conn = Conn { stream, next_id: DISPLAY };
    let registry = conn.alloc();
    conn.send(DISPLAY, 1, &[Arg::U(registry)])?;
    let mut manager = None;
    conn.roundtrip(|e| {
        if e.object == registry && e.opcode == 0 {
            // wl_registry.global: name, interface, version.
            if let (Some(name), Some((interface, at))) = (u32_at(&e.body, 0), string_at(&e.body, 4))
                && interface == "zwlr_virtual_pointer_manager_v1"
            {
                manager = Some((name, u32_at(&e.body, at).unwrap_or(1)));
            }
        }
    })?;
    let (name, version) = manager.ok_or_else(|| io::Error::other("the compositor has no virtual pointer (zwlr_virtual_pointer_manager_v1)"))?;
    let interface = "zwlr_virtual_pointer_manager_v1";
    let manager = conn.alloc();
    conn.send(registry, 0, &[Arg::U(name), Arg::S(interface), Arg::U(version.min(1)), Arg::U(manager)])?;
    // create_virtual_pointer with no seat: the compositor's default one.
    let pointer = conn.alloc();
    conn.send(manager, 0, &[Arg::U(0), Arg::U(pointer)])?;
    let [width, height] = plan.extent;
    for step in &plan.steps {
        let t = now_ms();
        match *step {
            Step::Move([x, y]) => {
                let x = x.clamp(0, i64::from(width.saturating_sub(1))) as u32;
                let y = y.clamp(0, i64::from(height.saturating_sub(1))) as u32;
                conn.send(pointer, 1, &[Arg::U(t), Arg::U(x), Arg::U(y), Arg::U(width), Arg::U(height)])?;
            }
            Step::Button { button, pressed } => conn.send(pointer, 2, &[Arg::U(t), Arg::U(button), Arg::U(u32::from(pressed))])?,
            Step::Axis { dx, dy } => {
                // As a mouse wheel: one click per WHEEL_POINTS.
                conn.send(pointer, 5, &[Arg::U(0)])?;
                for (axis, value) in [(0u32, dy), (1, dx)] {
                    if value != 0.0 {
                        let clicks = wheel_clicks(value);
                        conn.send(pointer, 7, &[Arg::U(t), Arg::U(axis), Arg::F(value), Arg::U(clicks as u32)])?;
                    }
                }
            }
            Step::Sleep(ms) => {
                conn.roundtrip(|_| {})?;
                std::thread::sleep(Duration::from_millis(ms.min(5000)));
                continue;
            }
        }
        conn.send(pointer, 4, &[])?; // frame
    }
    // With the device gone the seat loses its pointer, and toolkits drop
    // what they hadn't handled yet (GTK's animated wheel scroll): let them.
    conn.roundtrip(|_| {})?;
    std::thread::sleep(Duration::from_millis(LINGER_MS));
    conn.send(pointer, 8, &[])?; // destroy
    conn.roundtrip(|_| {})
}

/// How far one wheel click scrolls, in points: what libinput reports for a
/// mouse wheel (15 degrees a click).
const WHEEL_POINTS: f64 = 15.0;

/// Wheel clicks for a scroll of `value` points: at least one either way.
fn wheel_clicks(value: f64) -> i32 {
    let clicks = (value / WHEEL_POINTS).round() as i32;
    if clicks == 0 { value.signum() as i32 } else { clicks }
}

/// A move, `count` clicks of `button`, as plan steps.
pub fn click_steps(x: i64, y: i64, button: u32, count: u32) -> Vec<Step> {
    let mut steps = vec![Step::Move([x, y]), Step::Sleep(30)];
    for i in 0..count.clamp(1, 3) {
        if i > 0 {
            steps.push(Step::Sleep(40));
        }
        steps.push(Step::Button { button, pressed: true });
        steps.push(Step::Sleep(30));
        steps.push(Step::Button { button, pressed: false });
    }
    steps
}

/// A press at `from`, a glide to `to` at about 60 Hz, a release.
pub fn drag_steps(from: [i64; 2], to: [i64; 2]) -> Vec<Step> {
    let mut steps = vec![Step::Move(from), Step::Sleep(30), Step::Button { button: BTN_LEFT, pressed: true }, Step::Sleep(50)];
    let n = 12;
    for i in 1..=n {
        let x = from[0] + (to[0] - from[0]) * i / n;
        let y = from[1] + (to[1] - from[1]) * i / n;
        steps.push(Step::Move([x, y]));
        steps.push(Step::Sleep(16));
    }
    steps.push(Step::Sleep(50));
    steps.push(Step::Button { button: BTN_LEFT, pressed: false });
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_messages() {
        // wl_registry.bind(7, "wl_seat", 5, new id 9)
        let msg = encode(2, 0, &[Arg::U(7), Arg::S("wl_seat"), Arg::U(5), Arg::U(9)]);
        assert_eq!(msg.len(), 8 + 4 + (4 + 8) + 4 + 4);
        assert_eq!(u32_at(&msg, 0), Some(2));
        assert_eq!(u32_at(&msg, 4), Some(((msg.len() as u32) << 16) | 0));
        assert_eq!(string_at(&msg[8..], 4), Some(("wl_seat".to_string(), 16)));
        // Fixed point: 1.5 is 384.
        assert_eq!(u32_at(&encode(5, 3, &[Arg::F(1.5)]), 8), Some(384));
        assert_eq!(u32_at(&encode(5, 3, &[Arg::F(-2.0)]), 8), Some((-512i32) as u32));
    }

    #[test]
    fn plans_parse_and_build() {
        let plan: Plan = serde_json::from_str(r#"{"extent":[1280,800],"steps":[{"move":[10,20]},{"button":{"button":272,"pressed":true}},{"axis":{"dx":0,"dy":30}},{"sleep":5}]}"#).unwrap();
        assert_eq!(plan.steps[0], Step::Move([10, 20]));
        assert_eq!(plan.steps[1], Step::Button { button: BTN_LEFT, pressed: true });
        let clicks = click_steps(5, 5, BTN_LEFT, 2);
        assert_eq!(clicks.iter().filter(|s| matches!(s, Step::Button { pressed: true, .. })).count(), 2);
        let drag = drag_steps([0, 0], [120, 60]);
        assert!(drag.contains(&Step::Move([120, 60])));
        assert_eq!(drag.last(), Some(&Step::Button { button: BTN_LEFT, pressed: false }));
    }
}
