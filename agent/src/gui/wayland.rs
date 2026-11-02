//! A tiny Wayland client for pointer input: `berth-agent gui-pointer <plan>`,
//! run inside a session (as the machine's user, with its WAYLAND_DISPLAY).
//! Its connection (`Conn`) also carries the session streams: capture.rs
//! (wlr-screencopy into shared memory) and input.rs (a long-lived virtual
//! pointer and keyboard).
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
//! to 4. Everything is native-endian. File descriptors aren't in the bytes:
//! they travel beside them, as SCM_RIGHTS on the same sendmsg.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
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

pub(super) enum Arg<'a> {
    U(u32),
    I(i32),
    F(f64),
    S(&'a str),
    /// A file descriptor's place in the signature: it adds no bytes, and
    /// goes with the message (`Conn::send_with_fd`).
    Fd,
}

pub(super) fn encode(object: u32, opcode: u16, args: &[Arg]) -> Vec<u8> {
    let mut body = Vec::new();
    for arg in args {
        match arg {
            Arg::U(v) => body.extend_from_slice(&v.to_ne_bytes()),
            Arg::I(v) => body.extend_from_slice(&v.to_ne_bytes()),
            Arg::F(v) => body.extend_from_slice(&((v * 256.0).round() as i32).to_ne_bytes()),
            Arg::S(s) => {
                body.extend_from_slice(&(s.len() as u32 + 1).to_ne_bytes());
                body.extend_from_slice(s.as_bytes());
                body.push(0);
                while body.len() % 4 != 0 {
                    body.push(0);
                }
            }
            Arg::Fd => {}
        }
    }
    let mut msg = Vec::with_capacity(8 + body.len());
    msg.extend_from_slice(&object.to_ne_bytes());
    msg.extend_from_slice(&((((8 + body.len()) as u32) << 16) | u32::from(opcode)).to_ne_bytes());
    msg.extend_from_slice(&body);
    msg
}

pub(super) struct Event {
    pub object: u32,
    pub opcode: u16,
    pub body: Vec<u8>,
}

pub(super) fn u32_at(body: &[u8], at: usize) -> Option<u32> {
    body.get(at..at + 4).map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
}

/// A string argument at `at`: the string and where the next argument starts.
fn string_at(body: &[u8], at: usize) -> Option<(String, usize)> {
    let len = u32_at(body, at)? as usize;
    let bytes = body.get(at + 4..at + 4 + len)?;
    let s = String::from_utf8_lossy(bytes.strip_suffix(&[0]).unwrap_or(bytes)).into_owned();
    Some((s, at + 4 + len.div_ceil(4) * 4))
}

/// A wl_array argument at `at`: its bytes and where the next argument starts.
/// No event we read carries one yet (wl_keyboard.enter would).
#[allow(dead_code)]
pub(super) fn array_at(body: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let len = u32_at(body, at)? as usize;
    let bytes = body.get(at + 4..at + 4 + len)?;
    Some((bytes, at + 4 + len.div_ceil(4) * 4))
}

/// A global the compositor offers.
pub(super) struct Global {
    pub name: u32,
    pub interface: String,
    pub version: u32,
}

/// A global bound to an object of ours, at the version both sides speak.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Bound {
    pub id: u32,
    pub version: u32,
}

pub(super) struct Conn {
    stream: UnixStream,
    next_id: u32,
    /// Ids the compositor has let go of (wl_display.delete_id), handed out
    /// again first, as libwayland does: a capture makes an object per frame.
    free: Vec<u32>,
}

impl Conn {
    /// The session's compositor, from XDG_RUNTIME_DIR and WAYLAND_DISPLAY.
    pub fn connect() -> io::Result<Conn> {
        let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("no XDG_RUNTIME_DIR"))?;
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
        let path = if display.starts_with('/') { display } else { format!("{runtime}/{display}") };
        let stream = UnixStream::connect(&path).map_err(|e| io::Error::other(format!("{path}: {e}")))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Conn::over(stream))
    }

    /// A connection over a socket already open (tests: a fake compositor).
    pub fn over(stream: UnixStream) -> Conn {
        Conn { stream, next_id: DISPLAY, free: Vec::new() }
    }

    /// A new object id: a released one, else the next in sequence (the
    /// server accepts no other).
    pub fn alloc(&mut self) -> u32 {
        if let Some(id) = self.free.pop() {
            return id;
        }
        self.next_id += 1;
        self.next_id
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// How long a read may wait; None waits for ever.
    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    pub fn send(&mut self, object: u32, opcode: u16, args: &[Arg]) -> io::Result<()> {
        self.stream.write_all(&encode(object, opcode, args))
    }

    /// Messages encoded already.
    pub fn send_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)
    }

    /// One encoded message whose signature has an `Arg::Fd`, and that
    /// descriptor.
    pub fn send_with_fd(&mut self, bytes: &[u8], fd: RawFd) -> io::Result<()> {
        send_with_fd(&self.stream, bytes, fd)
    }

    /// One message from the compositor, or None for the display's own
    /// bookkeeping (delete_id), handled here; so are its errors.
    pub fn poll_event(&mut self) -> io::Result<Option<Event>> {
        let event = self.read_event()?;
        if event.object == DISPLAY {
            match event.opcode {
                0 => {
                    // wl_display.error: object, code, message.
                    let message = string_at(&event.body, 8).map(|(s, _)| s).unwrap_or_default();
                    return Err(io::Error::other(format!("Wayland error: {message}")));
                }
                1 => {
                    if let Some(id) = u32_at(&event.body, 0) {
                        self.free.push(id);
                    }
                    return Ok(None);
                }
                _ => {}
            }
        }
        Ok(Some(event))
    }

    /// The next message for one of our objects.
    pub fn next_event(&mut self) -> io::Result<Event> {
        loop {
            if let Some(event) = self.poll_event()? {
                return Ok(event);
            }
        }
    }

    /// Whatever the compositor has sent already, without waiting: an error
    /// it reported surfaces here, and its side of the socket never fills.
    pub fn drain(&mut self) -> io::Result<()> {
        while readable(self.fd(), 0)? {
            self.poll_event()?;
        }
        Ok(())
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
    pub fn roundtrip(&mut self, mut on_event: impl FnMut(&Event)) -> io::Result<()> {
        let callback = self.alloc();
        self.send(DISPLAY, 0, &[Arg::U(callback)])?;
        loop {
            let event = self.next_event()?;
            if event.object == callback && event.opcode == 0 {
                return Ok(());
            }
            on_event(&event);
        }
    }

    /// The registry and its globals, after one roundtrip.
    fn globals(&mut self) -> io::Result<(u32, Vec<Global>)> {
        let registry = self.alloc();
        self.send(DISPLAY, 1, &[Arg::U(registry)])?;
        let mut globals = Vec::new();
        self.roundtrip(|e| {
            if e.object == registry && e.opcode == 0 {
                // wl_registry.global: name, interface, version.
                if let (Some(name), Some((interface, at))) = (u32_at(&e.body, 0), string_at(&e.body, 4)) {
                    globals.push(Global { name, interface, version: u32_at(&e.body, at).unwrap_or(1) });
                }
            }
        })?;
        Ok((registry, globals))
    }

    /// Bind each wanted (interface, highest version we speak) the compositor
    /// offers, in order; None for those it doesn't.
    pub fn bind_globals(&mut self, wanted: &[(&str, u32)]) -> io::Result<Vec<Option<Bound>>> {
        let (registry, globals) = self.globals()?;
        let mut out = Vec::new();
        for &(interface, max) in wanted {
            let Some(global) = globals.iter().find(|g| g.interface == interface) else {
                out.push(None);
                continue;
            };
            let version = global.version.min(max);
            let id = self.alloc();
            self.send(registry, 0, &[Arg::U(global.name), Arg::S(interface), Arg::U(version), Arg::U(id)])?;
            out.push(Some(Bound { id, version }));
        }
        Ok(out)
    }
}

/// `bytes` in one sendmsg, with `fd` beside them as SCM_RIGHTS.
pub(super) fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> io::Result<()> {
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    // u64s keep the control buffer aligned for a cmsghdr.
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let sent = unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        libc::sendmsg(stream.as_raw_fd(), &msg, 0)
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    // The descriptor went with the first byte; if the socket took only part
    // of the message, the rest follows as plain bytes.
    let mut stream = stream;
    stream.write_all(&bytes[sent as usize..])
}

/// Whether `fd` has something to read (or hung up) within `timeout_ms`
/// (-1 waits).
pub(super) fn readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    loop {
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n >= 0 {
            return Ok(n > 0 && pfd.revents != 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Anonymous shared memory of `len` bytes, for wl_shm pools and keymaps: a
/// memfd on Linux, elsewhere (host tests) an unlinked temporary file.
pub(super) fn memfd(name: &str, len: usize) -> io::Result<OwnedFd> {
    let fd = open_anonymous(name)?;
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

#[cfg(target_os = "linux")]
fn open_anonymous(name: &str) -> io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    let name = std::ffi::CString::new(name).map_err(io::Error::other)?;
    let fd = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

#[cfg(not(target_os = "linux"))]
fn open_anonymous(name: &str) -> io::Result<OwnedFd> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!("{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file.into())
}

pub(super) fn now_ms() -> u32 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}

/// Carry out `plan` on the session's compositor.
pub fn run(plan: &Plan) -> io::Result<()> {
    let mut conn = Conn::connect()?;
    let manager = conn.bind_globals(&[("zwlr_virtual_pointer_manager_v1", 1)])?[0]
        .ok_or_else(|| io::Error::other("the compositor has no virtual pointer (zwlr_virtual_pointer_manager_v1)"))?
        .id;
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
pub(super) const WHEEL_POINTS: f64 = 15.0;

/// Wheel clicks for a scroll of `value` points: at least one either way.
pub(super) fn wheel_clicks(value: f64) -> i32 {
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

/// Native-endian words as bytes: hand-assembled messages for tests.
#[cfg(test)]
pub(super) fn words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_ne_bytes()).collect()
}

/// Receive one message and the descriptor beside it (the compositor's side
/// of `send_with_fd`, for tests).
#[cfg(test)]
pub(super) fn recv_with_fd(stream: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    use std::os::fd::FromRawFd;
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fd = None;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if !cmsg.is_null() && (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            fd = Some(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>())));
        }
    }
    Ok((n as usize, fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};

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
        // An fd adds nothing to the bytes; a signed int is two's complement.
        assert_eq!(encode(4, 0, &[Arg::U(9), Arg::Fd, Arg::I(-1)]), words(&[4, 16 << 16, 9, u32::MAX]));
    }

    #[test]
    fn decodes_arrays() {
        // A 5-byte array, padded to 8, then a uint.
        let body = [words(&[5]), vec![1, 2, 3, 4, 5, 0, 0, 0], words(&[77])].concat();
        let (bytes, next) = array_at(&body, 0).unwrap();
        assert_eq!((bytes, next), (&[1u8, 2, 3, 4, 5][..], 12));
        assert_eq!(u32_at(&body, next), Some(77));
        assert_eq!(array_at(&words(&[0]), 0), Some((&[][..], 4)));
        assert!(array_at(&words(&[9, 0]), 0).is_none(), "an array longer than the message");
    }

    #[test]
    fn sends_a_descriptor_beside_the_bytes() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let shared = memfd("berth-test", 64).unwrap();
        let msg = encode(3, 0, &[Arg::U(10), Arg::Fd, Arg::I(64)]);
        send_with_fd(&ours, &msg, shared.as_raw_fd()).unwrap();
        let mut buf = [0u8; 64];
        let (n, fd) = recv_with_fd(&theirs, &mut buf).unwrap();
        assert_eq!(&buf[..n], &msg[..]);
        // Write through the received descriptor, read back through ours.
        let mut received = std::fs::File::from(fd.expect("a descriptor"));
        received.write_all(b"through the fd").unwrap();
        let mut file = std::fs::File::from(shared);
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut back = [0u8; 14];
        file.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"through the fd");
        assert_eq!(file.metadata().unwrap().len(), 64);
    }

    #[test]
    fn ids_are_reused_after_delete_id() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut conn = Conn::over(ours);
        assert_eq!((conn.alloc(), conn.alloc()), (2, 3));
        // wl_display.delete_id(2), then an event for object 3.
        theirs.write_all(&encode(DISPLAY, 1, &[Arg::U(2)])).unwrap();
        theirs.write_all(&encode(3, 0, &[Arg::U(5)])).unwrap();
        let event = conn.next_event().unwrap();
        assert_eq!((event.object, event.opcode, u32_at(&event.body, 0)), (3, 0, Some(5)));
        assert_eq!((conn.alloc(), conn.alloc()), (2, 4));
        // A display error is an error.
        theirs.write_all(&encode(DISPLAY, 0, &[Arg::U(3), Arg::U(1), Arg::S("bad buffer")])).unwrap();
        assert!(conn.drain().unwrap_err().to_string().contains("bad buffer"));
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
