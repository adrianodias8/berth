//! The latest frame of every scanout, and the cursor, as the guest last
//! flushed them, for a host-side display window and screenshots still to
//! come (`latest`, `wait_newer`, `cursor` have no caller yet); today only
//! device.rs's debug dump reads them. The command server
//! (server.rs) writes here on SET_SCANOUT, RESOURCE_FLUSH and the cursor
//! commands; nothing here knows about virtio.
//!
//! Frames are BGRA, 4 bytes a pixel, rows packed (`stride == width * 4`),
//! with opaque alpha for formats that have none. The pixels sit behind an
//! `Arc`: a reader takes a frame without copying, and the next flush copies
//! only if a reader still holds the old one.
//!
//! Also a PNG encoder (`encode_png`), stored (uncompressed) deflate blocks
//! with CRC-32 and Adler-32, which needs no crate: a 1080p screenshot is
//! ~8 MB and takes milliseconds, which is what an agent's screenshot wants.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::protocol::{MAX_SCANOUTS, Rect};

/// One scanout's picture.
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// BGRA, rows of `width * 4` bytes.
    pub pixels: Arc<Vec<u8>>,
    /// Bumped on every change, across all scanouts: a reader can tell
    /// whether it has seen this one.
    pub sequence: u64,
}

impl Frame {
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn png(&self) -> Vec<u8> {
        encode_png(self.width, self.height, &self.pixels, self.stride())
    }
}

/// The cursor image (from UPDATE_CURSOR), BGRA like frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub pixels: Arc<Vec<u8>>,
}

/// Where the cursor is and what it looks like; `image` None hides it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub scanout: u32,
    pub x: u32,
    pub y: u32,
    pub image: Option<CursorImage>,
}

struct Inner {
    frames: [Option<Frame>; MAX_SCANOUTS],
    cursor: Cursor,
    sequence: u64,
}

/// The frames of one GPU device.
pub struct Frames {
    inner: Mutex<Inner>,
    changed: Condvar,
}

/// The VM's display (one GPU device per VM).
pub static DISPLAY: Frames = Frames::new();

/// The latest frame of `scanout`, or None if it's off. For the display and
/// app side: screenshots, peek.
#[allow(dead_code)] // for the display window, screenshots and peek
pub fn latest(scanout: u32) -> Option<Frame> {
    DISPLAY.latest(scanout)
}

impl Frames {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                frames: [const { None }; MAX_SCANOUTS],
                cursor: Cursor { scanout: 0, x: 0, y: 0, image: None },
                sequence: 0,
            }),
            changed: Condvar::new(),
        }
    }

    pub fn latest(&self, scanout: u32) -> Option<Frame> {
        self.inner.lock().unwrap().frames.get(scanout as usize)?.clone()
    }

    /// A frame of `scanout` newer than `after` (a sequence number), waiting
    /// up to `timeout` for one: lets a viewer follow the display without
    /// polling.
    #[allow(dead_code)] // for the display window
    pub fn wait_newer(&self, scanout: u32, after: u64, timeout: Duration) -> Option<Frame> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(Some(f)) = inner.frames.get(scanout as usize)
                && f.sequence > after
            {
                return Some(f.clone());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            inner = self.changed.wait_timeout(inner, left).unwrap().0;
        }
    }

    #[allow(dead_code)] // for the display window
    pub fn cursor(&self) -> Cursor {
        self.inner.lock().unwrap().cursor.clone()
    }

    /// Show `scanout` at `width` x `height` (SET_SCANOUT), keeping the old
    /// pixels when the size didn't change, and draw `rect` of it with `draw`.
    pub(crate) fn update(&self, scanout: u32, width: u32, height: u32, rect: Rect, draw: impl FnOnce(&mut [u8], usize)) {
        let i = scanout as usize;
        if i >= MAX_SCANOUTS {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.sequence += 1;
        let sequence = inner.sequence;
        let slot = &mut inner.frames[i];
        if slot.as_ref().is_none_or(|f| f.width != width || f.height != height) {
            *slot = Some(Frame {
                width,
                height,
                pixels: Arc::new(vec![0; width as usize * height as usize * 4]),
                sequence,
            });
        }
        let frame = slot.as_mut().unwrap();
        frame.sequence = sequence;
        if !rect.is_empty() {
            let stride = frame.stride();
            draw(Arc::make_mut(&mut frame.pixels).as_mut_slice(), stride);
        }
        drop(inner);
        self.changed.notify_all();
    }

    /// The scanout was disabled (SET_SCANOUT with resource 0, or reset).
    pub(crate) fn remove(&self, scanout: u32) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(slot) = inner.frames.get_mut(scanout as usize) {
            *slot = None;
        }
        inner.sequence += 1;
        drop(inner);
        self.changed.notify_all();
    }

    pub(crate) fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.frames = [const { None }; MAX_SCANOUTS];
        inner.cursor = Cursor::default();
        inner.sequence += 1;
        drop(inner);
        self.changed.notify_all();
    }

    pub(crate) fn set_cursor(&self, cursor: Cursor) {
        self.inner.lock().unwrap().cursor = cursor;
    }

    pub(crate) fn move_cursor(&self, scanout: u32, x: u32, y: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.cursor.scanout = scanout;
        inner.cursor.x = x;
        inner.cursor.y = y;
    }
}

/// CRC-32 (IEEE, as PNG chunks use).
fn crc32(chunks: &[&[u8]]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, entry) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *entry = c;
        }
        t
    });
    let mut c = 0xffff_ffffu32;
    for chunk in chunks {
        for &b in *chunk {
            c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xffff_ffff
}

/// Adler-32 (zlib's checksum), taking the modulo only every 5552 bytes,
/// the most that can't overflow.
struct Adler32 {
    a: u32,
    b: u32,
}

impl Adler32 {
    const MOD: u32 = 65521;
    fn new() -> Self {
        Self { a: 1, b: 0 }
    }
    fn update(&mut self, data: &[u8]) {
        for block in data.chunks(5552) {
            for &x in block {
                self.a += x as u32;
                self.b += self.a;
            }
            self.a %= Self::MOD;
            self.b %= Self::MOD;
        }
    }
    fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// A PNG (8-bit RGBA) of a BGRA image whose rows are `stride` bytes apart.
pub fn encode_png(width: u32, height: u32, bgra: &[u8], stride: usize) -> Vec<u8> {
    let row = width as usize * 4;
    assert!(stride >= row && bgra.len() >= stride * (height as usize).saturating_sub(1) + if height > 0 { row } else { 0 });
    // The raw image: each row is filter type 0 (none) then RGBA pixels.
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0);
        for px in bgra[y * stride..y * stride + row].as_chunks::<4>().0 {
            raw.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    // zlib: header (deflate, 32K window, no dictionary, fastest), stored
    // blocks of at most 65535 bytes, Adler-32 of the raw data.
    let mut z = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 16);
    z.extend_from_slice(&[0x78, 0x01]);
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        z.push(if blocks.peek().is_none() { 1 } else { 0 });
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let mut adler = Adler32::new();
    adler.update(&raw);
    z.extend_from_slice(&adler.finish().to_be_bytes());

    let mut png = Vec::with_capacity(z.len() + 64);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bits, RGBA, deflate, adaptive filters, no interlace
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(&[b"123456789"]), 0xcbf4_3926);
        assert_eq!(crc32(&[b"IEND"]), 0xae42_6082);
        let mut a = Adler32::new();
        a.update(b"Wikipedia");
        assert_eq!(a.finish(), 0x11e6_0398);
        // Long input crosses the deferred-modulo boundary.
        let big = vec![0xffu8; 100_000];
        let (mut a, mut b) = (1u64, 0u64);
        for &x in &big {
            a = (a + x as u64) % 65521;
            b = (b + a) % 65521;
        }
        let mut fast = Adler32::new();
        fast.update(&big);
        assert_eq!(fast.finish() as u64, (b << 16) | a);
    }

    /// Undo the stored deflate blocks by hand and check every field.
    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut pos = 8;
        let (mut w, mut h, mut idat) = (0, 0, Vec::new());
        while pos < png.len() {
            let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &png[pos + 4..pos + 8];
            let data = &png[pos + 8..pos + 8 + len];
            let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
            assert_eq!(crc32(&[kind, data]), crc);
            match kind {
                b"IHDR" => {
                    w = u32::from_be_bytes(data[0..4].try_into().unwrap());
                    h = u32::from_be_bytes(data[4..8].try_into().unwrap());
                    assert_eq!(&data[8..], &[8, 6, 0, 0, 0]);
                }
                b"IDAT" => idat.extend_from_slice(data),
                b"IEND" => assert!(data.is_empty()),
                _ => panic!("unexpected chunk"),
            }
            pos += 12 + len;
        }
        assert_eq!(&idat[..2], &[0x78, 0x01]);
        assert_eq!(u16::from_be_bytes([0x78, 0x01]) % 31, 0, "zlib header check bits");
        let mut raw = Vec::new();
        let mut p = 2;
        loop {
            let last = idat[p] & 1;
            assert_eq!(idat[p] >> 1, 0, "stored block");
            let len = u16::from_le_bytes([idat[p + 1], idat[p + 2]]);
            let nlen = u16::from_le_bytes([idat[p + 3], idat[p + 4]]);
            assert_eq!(len, !nlen);
            raw.extend_from_slice(&idat[p + 5..p + 5 + len as usize]);
            p += 5 + len as usize;
            if last == 1 {
                break;
            }
        }
        let mut adler = Adler32::new();
        adler.update(&raw);
        assert_eq!(u32::from_be_bytes(idat[p..p + 4].try_into().unwrap()), adler.finish());
        assert_eq!(p + 4, idat.len());
        (w, h, raw)
    }

    fn test_image(w: u32, h: u32) -> Vec<u8> {
        let mut bgra = Vec::new();
        for y in 0..h {
            for x in 0..w {
                bgra.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
            }
        }
        bgra
    }

    #[test]
    fn png_round_trips() {
        // Big enough for several stored blocks.
        let (w, h) = (300u32, 200u32);
        let bgra = test_image(w, h);
        let png = encode_png(w, h, &bgra, w as usize * 4);
        let (dw, dh, raw) = decode(&png);
        assert_eq!((dw, dh), (w, h));
        for y in 0..h as usize {
            let row = &raw[y * (w as usize * 4 + 1)..][..w as usize * 4 + 1];
            assert_eq!(row[0], 0);
            for x in 0..w as usize {
                let src = &bgra[(y * w as usize + x) * 4..][..4];
                assert_eq!(&row[1 + x * 4..1 + x * 4 + 4], &[src[2], src[1], src[0], src[3]]);
            }
        }
        // A padded stride reads only each row's pixels.
        let mut padded = Vec::new();
        for y in 0..h as usize {
            padded.extend_from_slice(&bgra[y * w as usize * 4..(y + 1) * w as usize * 4]);
            padded.extend_from_slice(&[0xaa; 12]);
        }
        assert_eq!(encode_png(w, h, &padded, w as usize * 4 + 12), png);
        // Empty image still decodes.
        decode(&encode_png(0, 0, &[], 0));
    }

    /// The real decoders agree: zlib (through python3) and sips, when the
    /// Mac has them (it always has sips).
    #[test]
    fn png_decodes_with_zlib_and_sips() {
        let (w, h) = (257u32, 129u32);
        let png = encode_png(w, h, &test_image(w, h), w as usize * 4);
        let dir = std::env::temp_dir().join(format!("berth-gpu-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("frame.png");
        std::fs::write(&path, &png).unwrap();
        let script = "import sys,zlib,struct\n\
            d=open(sys.argv[1],'rb').read();p=8;z=b''\n\
            while p<len(d):\n n=struct.unpack('>I',d[p:p+4])[0];k=d[p+4:p+8]\n if k==b'IDAT': z+=d[p+8:p+8+n]\n p+=12+n\n\
            r=zlib.decompress(z);print(len(r))";
        if let Ok(out) = std::process::Command::new("python3").args(["-I", "-c", script]).arg(&path).output() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Only the stub of a Mac without the command line tools is skipped.
            if !stderr.contains("xcode-select") {
                assert!(out.status.success(), "zlib couldn't read it: {stderr}");
                let n: usize = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
                assert_eq!(n, (w as usize * 4 + 1) * h as usize);
            }
        }
        if let Ok(out) = std::process::Command::new("sips").args(["-g", "pixelWidth", "-g", "pixelHeight"]).arg(&path).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains(&format!("pixelWidth: {w}")), "{text}");
            assert!(text.contains(&format!("pixelHeight: {h}")), "{text}");
        }
        // BERTH_GPU_PNG_KEEP=<path> keeps a copy, to try other decoders on.
        if let Some(keep) = std::env::var_os("BERTH_GPU_PNG_KEEP") {
            std::fs::copy(&path, keep).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frames_keep_the_latest_and_signal_readers() {
        let frames = Frames::new();
        assert!(frames.latest(0).is_none());
        frames.update(0, 4, 2, Rect::new(0, 0, 4, 2), |px, stride| {
            assert_eq!(stride, 16);
            px.fill(7);
        });
        let first = frames.latest(0).unwrap();
        assert_eq!((first.width, first.height, first.pixels.len()), (4, 2, 32));
        // A reader holding the old frame keeps its pixels.
        frames.update(0, 4, 2, Rect::new(0, 0, 1, 1), |px, _| px[0] = 9);
        let second = frames.latest(0).unwrap();
        assert_eq!(first.pixels[0], 7);
        assert_eq!(second.pixels[0], 9);
        assert!(second.sequence > first.sequence);
        assert!(frames.wait_newer(0, second.sequence, Duration::from_millis(10)).is_none());
        assert!(frames.wait_newer(0, first.sequence, Duration::from_millis(10)).is_some());
        // A size change starts a fresh frame.
        frames.update(0, 2, 2, Rect::default(), |_, _| unreachable!());
        assert_eq!(frames.latest(0).unwrap().pixels.len(), 16);
        frames.remove(0);
        assert!(frames.latest(0).is_none());
        frames.update(99, 1, 1, Rect::new(0, 0, 1, 1), |_, _| unreachable!());
    }

    #[test]
    fn waiting_wakes_on_update() {
        let frames = Arc::new(Frames::new());
        let f = frames.clone();
        let t = std::thread::spawn(move || f.wait_newer(1, 0, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        frames.update(1, 1, 1, Rect::new(0, 0, 1, 1), |px, _| px.fill(1));
        assert_eq!(t.join().unwrap().unwrap().pixels[0], 1);
    }
}
