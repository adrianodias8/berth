//! `berth-agent gui-capture <json>`, inside a session: the session's screen
//! as a stream of frame records on stdout, for the `screen.stream` verb
//! (docs/gui-protocol.md, Streams; the host's side is docs/display-protocol.md).
//!
//! A raw-wire client of wlr-screencopy (zwlr_screencopy_manager_v1, which
//! sway and cage offer). Each frame is captured into one wl_shm buffer of
//! ours, a memfd we keep mapped, with copy_with_damage: the compositor
//! answers when something changed and says what, so an idle screen costs
//! nothing and a moving window sends only its rectangles. ARGB8888 and
//! XRGB8888 are B, G, R, A in memory, which is the record's bgra8.
//!
//! A record is a JSON line, `{"seq","w","h","stride","format":"bgra8",
//! "damage":[[x,y,w,h],…],"payload_len"}`, then the rows of each damaged
//! rectangle in turn, packed (w*4 bytes a row, top to bottom). `stride` is a
//! full frame's packed row, w*4. The first frame, and the first after a
//! size change or a failed copy, is the full screen. Errors are a final
//! `{"error":"…"}` line. It ends when stdin closes (the agent's end of the
//! stream) or stdout does.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::wayland::{self, Arg, Conn, Event, encode, u32_at};

/// More damage rectangles than this in one frame are sent as their bounds.
const MAX_RECTS: usize = 16;
/// How long a failed capture waits before trying again.
const RETRY: Duration = Duration::from_millis(100);

/// wl_shm formats we read: both are B, G, R, A (or X) in memory.
const ARGB8888: u32 = 0;
const XRGB8888: u32 = 1;

#[derive(Deserialize, Default)]
pub struct Options {
    /// "bgra8" (pixels in the record) or "resource" (virtio-gpu resource ids,
    /// reserved for the dmabuf path).
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub max_fps: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One captured frame in the shared buffer.
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row in `pixels`.
    pub stride: usize,
    /// Rows run bottom to top in memory (screencopy's y_invert flag).
    pub y_invert: bool,
    /// XRGB: the fourth byte is undefined, and is sent as opaque.
    pub opaque: bool,
    pub pixels: &'a [u8],
}

/// The rectangles to send: the whole frame when `full`, else the
/// compositor's damage clipped to the frame, as its bounds past MAX_RECTS.
/// Damage is in the screen's coordinates, top-left origin, whatever the
/// buffer's row order.
pub fn damage(reported: &[Rect], width: u32, height: u32, full: bool) -> Vec<Rect> {
    if full {
        return vec![Rect { x: 0, y: 0, w: width, h: height }];
    }
    let mut out: Vec<Rect> = reported
        .iter()
        .filter_map(|r| {
            let (x0, y0) = (r.x.min(width), r.y.min(height));
            let (x1, y1) = (r.x.saturating_add(r.w).min(width), r.y.saturating_add(r.h).min(height));
            (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
        })
        .collect();
    if out.len() > MAX_RECTS {
        let x0 = out.iter().map(|r| r.x).min().unwrap_or(0);
        let y0 = out.iter().map(|r| r.y).min().unwrap_or(0);
        let x1 = out.iter().map(|r| r.x + r.w).max().unwrap_or(0);
        let y1 = out.iter().map(|r| r.y + r.h).max().unwrap_or(0);
        out = vec![Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }];
    }
    out
}

/// The record for `frame`'s `rects` (from `damage`): its JSON line, with the
/// newline, and its payload.
pub fn record(seq: u64, frame: &Frame, rects: &[Rect]) -> (String, Vec<u8>) {
    let mut payload = Vec::with_capacity(rects.iter().map(|r| r.w as usize * r.h as usize * 4).sum());
    for r in rects {
        for row in r.y..r.y + r.h {
            let src = if frame.y_invert { frame.height - 1 - row } else { row } as usize;
            let at = src * frame.stride + r.x as usize * 4;
            payload.extend_from_slice(&frame.pixels[at..at + r.w as usize * 4]);
        }
    }
    if frame.opaque {
        for px in payload.as_chunks_mut::<4>().0 {
            px[3] = 0xff;
        }
    }
    let damage: Vec<String> = rects.iter().map(|r| format!("[{},{},{},{}]", r.x, r.y, r.w, r.h)).collect();
    // By hand, to keep the documented key order.
    let line = format!(
        "{{\"seq\":{seq},\"w\":{},\"h\":{},\"stride\":{},\"format\":\"bgra8\",\"damage\":[{}],\"payload_len\":{}}}\n",
        frame.width,
        frame.height,
        frame.width * 4,
        damage.join(","),
        payload.len()
    );
    (line, payload)
}

/// An `{"error":…}` record.
pub fn error_line(message: &str) -> String {
    format!("{}\n", serde_json::json!({ "error": message }))
}

// ---- Requests ----

/// wl_shm.create_pool(new id, fd, size): the fd goes beside the bytes.
pub fn create_pool(shm: u32, pool: u32, size: i32) -> Vec<u8> {
    encode(shm, 0, &[Arg::U(pool), Arg::Fd, Arg::I(size)])
}

/// wl_shm_pool.create_buffer(new id, offset, width, height, stride, format).
pub fn create_buffer(pool: u32, buffer: u32, info: &BufferInfo) -> Vec<u8> {
    encode(pool, 0, &[Arg::U(buffer), Arg::I(0), Arg::I(info.width as i32), Arg::I(info.height as i32), Arg::I(info.stride as i32), Arg::U(info.format)])
}

/// zwlr_screencopy_manager_v1.capture_output(new frame, overlay_cursor 0,
/// output): the cursor stays out of the pixels (the host draws its own).
pub fn capture_output(manager: u32, frame: u32, output: u32) -> Vec<u8> {
    encode(manager, 0, &[Arg::U(frame), Arg::I(0), Arg::U(output)])
}

/// zwlr_screencopy_frame_v1.copy_with_damage(buffer).
pub fn copy_with_damage(frame: u32, buffer: u32) -> Vec<u8> {
    encode(frame, 2, &[Arg::U(buffer)])
}

// ---- The capture loop ----

/// What the compositor wants a frame's shm buffer to be (the frame's
/// `buffer` event).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BufferInfo {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Our wl_buffer and the memory under it, mapped for reading.
struct ShmBuffer {
    id: u32,
    info: BufferInfo,
    map: *mut u8,
    len: usize,
    _fd: OwnedFd,
}

impl ShmBuffer {
    fn new(conn: &mut Conn, shm: u32, info: BufferInfo) -> io::Result<ShmBuffer> {
        let len = info.stride as usize * info.height as usize;
        if info.width == 0 || (info.stride as usize) < info.width as usize * 4 || len > i32::MAX as usize {
            return Err(io::Error::other(format!("a {}x{} buffer with stride {}", info.width, info.height, info.stride)));
        }
        let fd = wayland::memfd("berth-capture", len)?;
        let map = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = ShmBuffer { id: 0, info, map: map.cast(), len, _fd: fd };
        let pool = conn.alloc();
        conn.send_with_fd(&create_pool(shm, pool, len as i32), buffer._fd.as_raw_fd())?;
        buffer.id = conn.alloc();
        conn.send_raw(&create_buffer(pool, buffer.id, &info))?;
        // The buffer keeps the pool's memory.
        conn.send(pool, 1, &[])?;
        Ok(buffer)
    }

    fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.map, self.len) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

/// The next event for us, or None once `stop` (stdin) has closed.
fn wait_event(conn: &mut Conn, stop: RawFd) -> io::Result<Option<Event>> {
    loop {
        let mut fds = [libc::pollfd { fd: conn.fd(), events: libc::POLLIN, revents: 0 }, libc::pollfd { fd: stop, events: libc::POLLIN, revents: 0 }];
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if fds[1].revents != 0 {
            // Nothing is expected on stdin: anything there is dropped, and
            // its end (or an error) ends the capture.
            let mut scratch = [0u8; 256];
            if unsafe { libc::read(stop, scratch.as_mut_ptr().cast(), scratch.len()) } <= 0 {
                return Ok(None);
            }
        }
        if fds[0].revents != 0
            && let Some(event) = conn.poll_event()?
        {
            return Ok(Some(event));
        }
    }
}

enum Copied {
    Ready { y_invert: bool, damage: Vec<Rect> },
    Failed,
    /// stdin closed.
    Stop,
}

/// Capture the session's screen into `out` until stdin or `out` closes.
pub fn run(out: &mut dyn Write, opts: &Options) -> io::Result<()> {
    match opts.format.as_deref().unwrap_or("bgra8") {
        "bgra8" => {}
        "resource" => return Err(io::Error::other("not implemented")),
        other => return Err(io::Error::other(format!("format {other:?} (bgra8)"))),
    }
    capture(Conn::connect()?, 0, out, opts)
}

/// `run` on a connection, until `stop` closes.
fn capture(mut conn: Conn, stop: RawFd, out: &mut dyn Write, opts: &Options) -> io::Result<()> {
    let period = Duration::from_secs_f64(1.0 / f64::from(opts.max_fps.unwrap_or(60).clamp(1, 120)));
    let bound = conn.bind_globals(&[("wl_shm", 1), ("wl_output", 1), ("zwlr_screencopy_manager_v1", 3)])?;
    let (Some(shm), Some(output)) = (bound[0], bound[1]) else {
        return Err(io::Error::other("the compositor has no wl_shm or wl_output"));
    };
    let manager = bound[2].filter(|m| m.version >= 2).ok_or_else(|| io::Error::other("the compositor has no screencopy with damage (zwlr_screencopy_manager_v1 v2)"))?;
    conn.roundtrip(|_| {})?;
    // From here on, waiting for damage may take as long as the screen
    // stays still.
    conn.set_timeout(None)?;
    let mut buffer: Option<ShmBuffer> = None;
    let mut seq = 0u64;
    let mut full = true;
    let mut next = Instant::now();
    loop {
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next = Instant::now() + period;
        let frame = conn.alloc();
        conn.send_raw(&capture_output(manager.id, frame, output.id))?;
        // Its buffer events: the shm one, then (v3) buffer_done.
        let mut info = None;
        let mut failed = false;
        loop {
            let Some(e) = wait_event(&mut conn, stop)? else { return Ok(()) };
            if e.object != frame {
                continue;
            }
            match e.opcode {
                0 => {
                    let n = |i: usize| u32_at(&e.body, i * 4).unwrap_or(0);
                    info = Some(BufferInfo { format: n(0), width: n(1), height: n(2), stride: n(3) });
                    if manager.version < 3 {
                        break;
                    }
                }
                3 => {
                    failed = true;
                    break;
                }
                6 => break,
                _ => {}
            }
        }
        let Some(info) = info.filter(|_| !failed) else {
            conn.send(frame, 1, &[])?;
            full = true;
            std::thread::sleep(RETRY);
            continue;
        };
        if info.format != ARGB8888 && info.format != XRGB8888 {
            return Err(io::Error::other(format!("the compositor offers wl_shm format {:#x}, not ARGB/XRGB8888", info.format)));
        }
        if buffer.as_ref().is_none_or(|b| b.info != info) {
            if let Some(old) = buffer.take() {
                conn.send(old.id, 0, &[])?;
            }
            buffer = Some(ShmBuffer::new(&mut conn, shm.id, info)?);
            full = true;
        }
        let buf = buffer.as_ref().unwrap();
        conn.send_raw(&copy_with_damage(frame, buf.id))?;
        let copied = copy_events(&mut conn, stop, frame)?;
        conn.send(frame, 1, &[])?; // destroy
        let (y_invert, reported) = match copied {
            Copied::Ready { y_invert, damage } => (y_invert, damage),
            Copied::Failed => {
                full = true;
                std::thread::sleep(RETRY);
                continue;
            }
            Copied::Stop => return Ok(()),
        };
        let rects = damage(&reported, info.width, info.height, full);
        if rects.is_empty() {
            continue;
        }
        let shot = Frame { width: info.width, height: info.height, stride: info.stride as usize, y_invert, opaque: info.format == XRGB8888, pixels: buf.pixels() };
        seq += 1;
        let (line, payload) = record(seq, &shot, &rects);
        out.write_all(line.as_bytes())?;
        out.write_all(&payload)?;
        out.flush()?;
        full = false;
    }
}

/// A copy's events up to ready or failed.
fn copy_events(conn: &mut Conn, stop: RawFd, frame: u32) -> io::Result<Copied> {
    let mut y_invert = false;
    let mut damage = Vec::new();
    loop {
        let Some(e) = wait_event(conn, stop)? else { return Ok(Copied::Stop) };
        if e.object != frame {
            continue; // wl_buffer.release, wl_shm.format, wl_output's
        }
        let n = |i: usize| u32_at(&e.body, i * 4).unwrap_or(0);
        match e.opcode {
            1 => y_invert = n(0) & 1 != 0,
            2 => return Ok(Copied::Ready { y_invert, damage }),
            3 => return Ok(Copied::Failed),
            4 => damage.push(Rect { x: n(0), y: n(1), w: n(2), h: n(3) }),
            _ => {}
        }
    }
}

/// `berth-agent gui-capture <options json>`.
pub fn local(args: &[String]) -> i32 {
    let opts = match args.first().map(|j| if j.trim() == "null" { Ok(Options::default()) } else { serde_json::from_str::<Options>(j) }) {
        Some(Ok(opts)) => opts,
        _ => {
            eprintln!("usage: berth-agent gui-capture '{{\"format\":\"bgra8\",\"max_fps\":60}}'");
            return 2;
        }
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match run(&mut out, &opts) {
        Ok(()) => 0,
        Err(err) => {
            // Its last word on the stream, unless stdout is what failed.
            let _ = out.write_all(error_line(&err.to_string()).as_bytes()).and_then(|()| out.flush());
            1
        }
    }
}

/// The virtio-gpu ioctls a dmabuf capture will use to name its buffers by
/// resource id (records with `"format":"resource"`); reserved, and pinned
/// here against the kernel's uapi (drm/virtgpu_drm.h, drm/drm.h).
#[allow(dead_code)]
pub mod virtgpu {
    #[repr(C)]
    #[derive(Default)]
    pub struct ResourceCreate {
        pub target: u32,
        pub format: u32,
        pub bind: u32,
        pub width: u32,
        pub height: u32,
        pub depth: u32,
        pub array_size: u32,
        pub last_level: u32,
        pub nr_samples: u32,
        pub flags: u32,
        pub bo_handle: u32,
        pub res_handle: u32,
        pub size: u32,
        pub stride: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct ResourceInfo {
        pub bo_handle: u32,
        pub res_handle: u32,
        pub size: u32,
        pub blob_mem: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct PrimeHandle {
        pub handle: u32,
        pub flags: u32,
        pub fd: i32,
    }

    /// _IOWR('d', nr, size).
    const fn iowr(nr: u32, size: usize) -> u32 {
        (3 << 30) | ((size as u32) << 16) | ((b'd' as u32) << 8) | nr
    }

    const DRM_COMMAND_BASE: u32 = 0x40;
    pub const RESOURCE_CREATE: u32 = iowr(DRM_COMMAND_BASE + 0x04, size_of::<ResourceCreate>());
    pub const RESOURCE_INFO: u32 = iowr(DRM_COMMAND_BASE + 0x05, size_of::<ResourceInfo>());
    pub const PRIME_HANDLE_TO_FD: u32 = iowr(0x2d, size_of::<PrimeHandle>());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::wayland::words;

    /// An 8x4 frame whose pixel (x, y) is [x, y, row-in-memory, 0x80].
    fn frame_bytes(y_invert: bool) -> Vec<u8> {
        let (w, h, stride) = (8u32, 4u32, 40usize);
        let mut px = vec![0u8; stride * h as usize];
        for mem_row in 0..h {
            let y = if y_invert { h - 1 - mem_row } else { mem_row };
            for x in 0..w {
                let at = mem_row as usize * stride + x as usize * 4;
                px[at..at + 4].copy_from_slice(&[x as u8, y as u8, mem_row as u8, 0x80]);
            }
        }
        px
    }

    #[test]
    fn records_carry_the_damaged_rows() {
        let pixels = frame_bytes(false);
        let frame = Frame { width: 8, height: 4, stride: 40, y_invert: false, opaque: false, pixels: &pixels };
        let rects = damage(&[Rect { x: 1, y: 1, w: 2, h: 2 }], 8, 4, false);
        let (line, payload) = record(7, &frame, &rects);
        assert_eq!(line, "{\"seq\":7,\"w\":8,\"h\":4,\"stride\":32,\"format\":\"bgra8\",\"damage\":[[1,1,2,2]],\"payload_len\":16}\n");
        assert_eq!(payload, [[1, 1, 1, 0x80], [2, 1, 1, 0x80], [1, 2, 2, 0x80], [2, 2, 2, 0x80]].concat());
    }

    #[test]
    fn the_first_frame_is_full() {
        let pixels = frame_bytes(false);
        let frame = Frame { width: 8, height: 4, stride: 40, y_invert: false, opaque: true, pixels: &pixels };
        let rects = damage(&[Rect { x: 1, y: 1, w: 2, h: 2 }], 8, 4, true);
        assert_eq!(rects, [Rect { x: 0, y: 0, w: 8, h: 4 }]);
        let (line, payload) = record(1, &frame, &rects);
        assert!(line.contains("\"damage\":[[0,0,8,4]],\"payload_len\":128}"), "{line}");
        // Packed rows (the 8 bytes of stride padding dropped), XRGB opaque.
        assert_eq!(&payload[32..36], &[0, 1, 1, 0xff]);
        assert!(payload.chunks(4).all(|p| p[3] == 0xff));
    }

    #[test]
    fn y_invert_flips_the_rows() {
        let pixels = frame_bytes(true);
        let frame = Frame { width: 8, height: 4, stride: 40, y_invert: true, opaque: false, pixels: &pixels };
        let (_, payload) = record(1, &frame, &damage(&[], 8, 4, true));
        // Screen row y comes from memory row 3 - y, top to bottom.
        for y in 0..4u8 {
            let row = &payload[y as usize * 32..y as usize * 32 + 4];
            assert_eq!(row, &[0, y, 3 - y, 0x80]);
        }
    }

    #[test]
    fn damage_is_clipped_and_bounded() {
        let r = |x, y, w, h| Rect { x, y, w, h };
        assert_eq!(damage(&[r(6, 2, 10, 10), r(9, 0, 1, 1), r(0, 0, 0, 3)], 8, 4, false), [r(6, 2, 2, 2)]);
        let many: Vec<Rect> = (0..20).map(|i| r(i, i / 2, 1, 1)).collect();
        assert_eq!(damage(&many, 100, 100, false), [r(0, 0, 20, 10)]);
        assert_eq!(damage(&[r(u32::MAX, 0, u32::MAX, 1)], 8, 4, false), []);
    }

    #[test]
    fn requests_on_the_wire() {
        // wl_shm.create_pool(new id 10, fd, 4096): no bytes for the fd.
        assert_eq!(create_pool(4, 10, 4096), words(&[4, 16 << 16, 10, 4096]));
        let info = BufferInfo { format: XRGB8888, width: 1280, height: 800, stride: 5120 };
        assert_eq!(create_buffer(10, 11, &info), words(&[10, 32 << 16, 11, 0, 1280, 800, 5120, 1]));
        // capture_output(new frame 12, no cursor, output 5); copy_with_damage is opcode 2.
        assert_eq!(capture_output(6, 12, 5), words(&[6, 20 << 16, 12, 0, 5]));
        assert_eq!(copy_with_damage(12, 11), words(&[12, (12 << 16) | 2, 11]));
    }

    #[test]
    fn options_and_errors() {
        let opts: Options = serde_json::from_str(r#"{"format":"resource","max_fps":30}"#).unwrap();
        assert_eq!((opts.format.as_deref(), opts.max_fps), (Some("resource"), Some(30)));
        let mut out = Vec::new();
        assert_eq!(run(&mut out, &opts).unwrap_err().to_string(), "not implemented");
        assert_eq!(error_line("not implemented"), "{\"error\":\"not implemented\"}\n");
    }

    /// The compositor's end of a connection: our requests in order, and the
    /// descriptors that came with them.
    struct Fake {
        stream: std::os::unix::net::UnixStream,
        buf: Vec<u8>,
        fds: Vec<OwnedFd>,
    }

    impl Fake {
        fn next(&mut self) -> (u32, u16, Vec<u8>) {
            loop {
                if self.buf.len() >= 8 {
                    let size = (u32_at(&self.buf, 4).unwrap() >> 16) as usize;
                    if self.buf.len() >= size {
                        let msg: Vec<u8> = self.buf.drain(..size).collect();
                        return (u32_at(&msg, 0).unwrap(), (u32_at(&msg, 4).unwrap() & 0xffff) as u16, msg[8..].to_vec());
                    }
                }
                let mut chunk = [0u8; 4096];
                let (n, fd) = crate::gui::wayland::recv_with_fd(&self.stream, &mut chunk).unwrap();
                assert!(n > 0, "the client hung up");
                self.buf.extend_from_slice(&chunk[..n]);
                self.fds.extend(fd);
            }
        }

        /// The next request, which must be `opcode` on `object`: its arguments.
        fn expect(&mut self, object: u32, opcode: u16) -> Vec<u32> {
            let (o, op, body) = self.next();
            assert_eq!((o, op), (object, opcode), "request");
            body.chunks(4).map(|w| u32::from_ne_bytes(w.try_into().unwrap())).collect()
        }

        fn send(&mut self, object: u32, opcode: u16, args: &[Arg]) {
            self.stream.write_all(&encode(object, opcode, args)).unwrap();
        }

        /// Answer a wl_display.sync.
        fn sync(&mut self) {
            let callback = self.expect(1, 0)[0];
            self.send(callback, 0, &[Arg::U(0)]);
            self.send(1, 1, &[Arg::U(callback)]);
        }
    }

    #[test]
    fn captures_from_a_compositor() {
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::FileExt;
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let (stop, stop_writer) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        let compositor = std::thread::spawn(move || {
            let mut c = Fake { stream: theirs, buf: Vec::new(), fds: Vec::new() };
            let registry = c.expect(1, 1)[0];
            for (name, interface, version) in [(1, "wl_shm", 1), (2, "wl_output", 3), (3, "zwlr_screencopy_manager_v1", 3)] {
                c.send(registry, 0, &[Arg::U(name), Arg::S(interface), Arg::U(version)]);
            }
            c.sync();
            // Binds: name, interface (length, 3 words of "wl_shm\0"…), version, new id.
            let shm = *c.expect(registry, 0).last().unwrap();
            let output = *c.expect(registry, 0).last().unwrap();
            let bind = c.expect(registry, 0);
            assert_eq!(bind[bind.len() - 2], 3, "screencopy at v3");
            let manager = *bind.last().unwrap();
            c.sync();
            // First frame: an 8x4 XRGB buffer with a 40-byte stride.
            let frame = c.expect(manager, 0);
            assert_eq!((frame[1], frame[2]), (0, output), "no cursor, our output");
            c.send(frame[0], 0, &[Arg::U(XRGB8888), Arg::U(8), Arg::U(4), Arg::U(40)]);
            c.send(frame[0], 6, &[]);
            let pool = c.expect(shm, 0);
            assert_eq!(pool[1], 160);
            let memory = std::fs::File::from(c.fds.pop().expect("the pool's fd"));
            let buffer = c.expect(pool[0], 0);
            assert_eq!(&buffer[1..], &[0, 8, 4, 40, XRGB8888]);
            c.expect(pool[0], 1);
            assert_eq!(c.expect(frame[0], 2), [buffer[0]]);
            for y in 0..4u8 {
                for x in 0..8u8 {
                    memory.write_at(&[x, y, 0x10, 0], u64::from(y) * 40 + u64::from(x) * 4).unwrap();
                }
            }
            c.send(frame[0], 1, &[Arg::U(0)]);
            c.send(frame[0], 4, &[Arg::U(1), Arg::U(1), Arg::U(2), Arg::U(2)]);
            c.send(frame[0], 2, &[Arg::U(0), Arg::U(1), Arg::U(0)]);
            c.expect(frame[0], 1);
            c.send(1, 1, &[Arg::U(frame[0])]);
            // Second frame: one pixel changed, into the same buffer.
            let frame = c.expect(manager, 0);
            c.send(frame[0], 0, &[Arg::U(XRGB8888), Arg::U(8), Arg::U(4), Arg::U(40)]);
            c.send(frame[0], 6, &[]);
            assert_eq!(c.expect(frame[0], 2), [buffer[0]], "the buffer is reused");
            memory.write_at(&[9, 9, 9, 0], 2 * 40 + 2 * 4).unwrap();
            c.send(frame[0], 4, &[Arg::U(2), Arg::U(2), Arg::U(1), Arg::U(1)]);
            c.send(frame[0], 2, &[Arg::U(0), Arg::U(2), Arg::U(0)]);
            c.expect(frame[0], 1);
            // The agent's end of the stream closes; the compositor stays.
            drop(stop_writer);
            c
        });
        let mut out = Vec::new();
        let opts = Options { format: None, max_fps: Some(120) };
        capture(Conn::over(ours), stop.as_raw_fd(), &mut out, &opts).unwrap();
        compositor.join().unwrap();
        let first = "{\"seq\":1,\"w\":8,\"h\":4,\"stride\":32,\"format\":\"bgra8\",\"damage\":[[0,0,8,4]],\"payload_len\":128}\n";
        assert!(out.starts_with(first.as_bytes()), "{}", String::from_utf8_lossy(&out[..out.len().min(200)]));
        let payload = &out[first.len()..first.len() + 128];
        assert_eq!((&payload[0..4], &payload[32 * 3 + 28..32 * 3 + 32]), (&[0, 0, 0x10, 0xff][..], &[7, 3, 0x10, 0xff][..]));
        let rest = &out[first.len() + 128..];
        let second = "{\"seq\":2,\"w\":8,\"h\":4,\"stride\":32,\"format\":\"bgra8\",\"damage\":[[2,2,1,1]],\"payload_len\":4}\n";
        assert_eq!(rest, [second.as_bytes(), &[9, 9, 9, 0xff]].concat());
    }

    #[test]
    fn virtgpu_ioctls_are_pinned() {
        assert_eq!(size_of::<virtgpu::ResourceCreate>(), 56);
        assert_eq!(size_of::<virtgpu::ResourceInfo>(), 16);
        assert_eq!(size_of::<virtgpu::PrimeHandle>(), 12);
        assert_eq!(virtgpu::RESOURCE_CREATE, 0xC038_6444);
        assert_eq!(virtgpu::RESOURCE_INFO, 0xC010_6445);
        assert_eq!(virtgpu::PRIME_HANDLE_TO_FD, 0xC00C_642D);
    }
}
