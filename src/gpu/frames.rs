//! Every surface berth shows, with its latest frame and cursor: a VM
//! scanout as the GPU device last flushed it (server.rs writes those on
//! SET_SCANOUT, RESOURCE_FLUSH and the cursor commands), or a guest GUI
//! session's screen as its capture stream sends it (gui/display.rs).
//! Nothing here knows about virtio or the guest.
//!
//! Each surface's pixels live in a ring (ring.rs): a file in
//! ~/.berth/run/surfaces that Berth.app maps, so frames reach it without a
//! copy. Here, under one lock, is everything else: the frame sequence, a
//! short history of what each frame changed (so a viewer that skipped
//! frames gets the union of their damage), the cursor, and a condvar that
//! `wait_newer` sleeps on. The display stream (docs/display-protocol.md)
//! is built on `wait_newer` and `info`; screenshots on `png`.
//!
//! `DISPLAY` is the GPU device's view of the registry: scanout `n` is the
//! surface `Scanout(n)`, behind the same calls the device has always made.
//!
//! Frames are BGRA with opaque alpha for formats that have none. Also a PNG
//! encoder (`encode_png`), stored (uncompressed) deflate blocks with CRC-32
//! and Adler-32, which needs no crate: a 1080p screenshot is ~8 MB and
//! takes milliseconds, which is what an agent's screenshot wants.

#[path = "ring.rs"]
pub mod ring;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::protocol::{MAX_SCANOUTS, Rect};
use ring::{CursorState, Ring};

/// How many frames' damage a surface remembers. A viewer further behind
/// than that gets a full frame.
pub const DAMAGE_HISTORY: usize = 16;

/// One surface's picture, packed (`stride == width * 4`).
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// BGRA, rows of `width * 4` bytes.
    pub pixels: Arc<Vec<u8>>,
    /// Bumped on every frame, across all surfaces: a reader can tell
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

/// What a surface shows.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SurfaceKey {
    /// A guest GUI session's screen, fed by its capture stream.
    Session { machine: String, session: String },
    /// A scanout of the VM's GPU device.
    Scanout(u32),
}

impl SurfaceKey {
    /// The start of its ring's file name (the gen follows).
    pub fn stem(&self) -> String {
        match self {
            SurfaceKey::Session { machine, session } => format!("session-{machine}-{session}"),
            SurfaceKey::Scanout(n) => format!("scanout-{n}"),
        }
    }
}

impl std::fmt::Display for SurfaceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SurfaceKey::Session { machine, session } => write!(f, "{machine} session {session}"),
            SurfaceKey::Scanout(n) => write!(f, "scanout {n}"),
        }
    }
}

/// A live surface's ring, as the display stream's reply describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    /// None for an anonymous ring (no surfaces directory set: tests).
    pub path: Option<PathBuf>,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// What a viewer has had so far: `wait_newer` answers with what's newer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// The ring generation (0: none).
    pub generation: u64,
    pub frame: u64,
    pub cursor: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    /// Slot `slot` holds frame `seq`. `damage` covers everything that
    /// changed since the frame the viewer saw: the whole surface (`full`)
    /// after a resize or when the history doesn't reach back that far.
    Frame { slot: u32, seq: u64, damage: Rect, full: bool },
    Cursor { seq: u64 },
    /// A new ring generation, or None: no live surface any more.
    Resize(Option<Info>),
    /// The surface's source failed (its capture stream ended).
    End(String),
    Timeout,
}

#[derive(Default)]
struct Surface {
    ring: Option<Ring>,
    /// The latest frame's sequence (0: none in this ring).
    seq: u64,
    /// What each recent frame of this ring changed, oldest first.
    damage: VecDeque<(u64, Rect)>,
    /// Frames up to here have left the history (or are from an older
    /// ring): a viewer that saw only those needs a full frame.
    floor: u64,
    cursor: CursorState,
    cursor_image: Option<CursorImage>,
    cursor_seq: u64,
    ended: Option<String>,
}

impl Surface {
    fn info(&self) -> Option<Info> {
        let r = self.ring.as_ref()?;
        Some(Info { path: r.path().map(Path::to_path_buf), generation: r.generation(), width: r.width(), height: r.height(), stride: r.stride() as u32 })
    }

    /// The union of what changed after frame `after`, or None when the
    /// history doesn't reach back to it.
    fn damage_since(&self, after: u64) -> Option<Rect> {
        if after < self.floor || after == 0 {
            return None;
        }
        Some(self.damage.iter().filter(|(s, _)| *s > after).fold(Rect::default(), |u, (_, r)| union(u, *r)))
    }

    fn write_cursor(&mut self, seq: u64) {
        let image = self.cursor_image.as_ref().map(|i| i.pixels.as_slice());
        if let Some(ring) = &mut self.ring {
            ring.write_cursor(seq, self.cursor, image);
        }
    }
}

struct Inner {
    dir: Option<PathBuf>,
    map: BTreeMap<SurfaceKey, Surface>,
    /// Frame and cursor sequence numbers, shared by all surfaces.
    sequence: u64,
    /// Ring generations, 1..=u32::MAX, unique across daemon runs (seeded
    /// from the clock).
    generation: u64,
}

/// A registry of surfaces.
pub struct Surfaces {
    inner: Mutex<Inner>,
    changed: Condvar,
}

/// All of berth's surfaces: the VM's scanouts and the GUI sessions' screens.
pub static SURFACES: Surfaces = Surfaces::new();

/// The VM's display (one GPU device per VM), onto `SURFACES`.
pub static DISPLAY: Frames = Frames::on(&SURFACES);

/// The bounding box of two rectangles (an empty one adds nothing).
pub fn union(a: Rect, b: Rect) -> Rect {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let (x0, y0) = (a.x.min(b.x), a.y.min(b.y));
    let x1 = (a.x as u64 + a.width as u64).max(b.x as u64 + b.width as u64);
    let y1 = (a.y as u64 + a.height as u64).max(b.y as u64 + b.height as u64);
    Rect::new(x0, y0, (x1 - x0 as u64) as u32, (y1 - y0 as u64) as u32)
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.is_empty()
        || (inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x as u64 + inner.width as u64 <= outer.x as u64 + outer.width as u64
            && inner.y as u64 + inner.height as u64 <= outer.y as u64 + outer.height as u64)
}

impl Default for Surfaces {
    fn default() -> Self {
        Self::new()
    }
}

impl Surfaces {
    /// Without a directory (`set_dir`), rings are anonymous memory.
    pub const fn new() -> Self {
        Self { inner: Mutex::new(Inner { dir: None, map: BTreeMap::new(), sequence: 0, generation: 0 }), changed: Condvar::new() }
    }

    /// Put rings in `dir` from now on (made 0700), removing any a previous
    /// daemon left there.
    pub fn set_dir(&self, dir: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        for entry in std::fs::read_dir(dir)?.flatten() {
            if entry.path().extension().is_some_and(|e| e == "ring") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        self.inner.lock().unwrap().dir = Some(dir.to_path_buf());
        Ok(())
    }

    /// Show `key` at `width` x `height` and draw `rect` of it with `draw`,
    /// given the frame's bytes and stride: it redraws all of `rect` and
    /// nothing outside it. A new size is a new ring generation, starting
    /// black. An empty `rect` at the same size is no new frame.
    pub fn update(&self, key: &SurfaceKey, width: u32, height: u32, rect: Rect, draw: impl FnOnce(&mut [u8], usize)) {
        self.update_parts(key, width, height, &[rect], draw);
    }

    /// `update` for a frame that redraws several rectangles: its damage is
    /// their bounding box.
    pub fn update_parts(&self, key: &SurfaceKey, width: u32, height: u32, parts: &[Rect], draw: impl FnOnce(&mut [u8], usize)) {
        if ring::Layout::new(width, height).is_none() {
            return;
        }
        let rect = parts.iter().fold(Rect::default(), |u, r| union(u, *r));
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;
        let s = inner.map.entry(key.clone()).or_default();
        let resized = s.ring.as_ref().is_none_or(|r| r.width() != width || r.height() != height);
        if resized {
            // The header holds a u32 gen (so does the app's reply parser):
            // seconds since 1970 fit, and a later daemon starts above
            // whatever this one reached unless it resized more than once a
            // second.
            if inner.generation == 0 {
                inner.generation = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_secs() & u32::MAX as u64);
            }
            inner.generation = inner.generation % u32::MAX as u64 + 1;
            let ring = Ring::create(inner.dir.as_deref(), &key.stem(), inner.generation, width, height).or_else(|err| {
                eprintln!("display: can't share {key}'s frames ({err}); keeping them in memory");
                Ring::create(None, &key.stem(), inner.generation, width, height)
            });
            let Ok(ring) = ring else { return };
            // The old ring (and its file) goes now; viewers still mapping it
            // keep their pages.
            s.ring = Some(ring);
            s.damage.clear();
            s.seq = 0;
            // A cursor set before carries over into the new ring.
            if s.cursor_seq > 0 {
                inner.sequence += 1;
                s.write_cursor(inner.sequence);
                s.cursor_seq = inner.sequence;
            }
        } else if rect.is_empty() {
            return;
        }
        inner.sequence += 1;
        let seq = inner.sequence;
        let full = Rect::new(0, 0, width, height);
        let damage = if resized { full } else { rect.intersect(&full).unwrap_or_default() };
        if resized {
            s.floor = seq - 1;
        }
        let ring = s.ring.as_mut().unwrap();
        // The slot about to be written holds an older frame: bring it up to
        // date with what changed since, unless this frame redraws all of it.
        let slot_frame = ring.slot_frame(ring.next_slot());
        let catch_up = match s.damage.back() {
            None => None,
            Some(_) if slot_frame == 0 || slot_frame < s.floor => Some(full),
            Some(_) => Some(s.damage.iter().filter(|(q, _)| *q > slot_frame).fold(Rect::default(), |u, (_, r)| union(u, *r))),
        }
        .filter(|c| !parts.iter().any(|p| contains(*p, *c)));
        ring.write(seq, catch_up, |px, stride| {
            if !rect.is_empty() {
                draw(px, stride)
            }
        });
        s.seq = seq;
        s.damage.push_back((seq, damage));
        while s.damage.len() > DAMAGE_HISTORY {
            s.floor = s.damage.pop_front().unwrap().0;
        }
        self.changed.notify_all();
    }

    /// The surface's latest frame, copied out packed.
    pub fn frame(&self, key: &SurfaceKey) -> Option<Frame> {
        let inner = self.inner.lock().unwrap();
        let s = inner.map.get(key)?;
        let ring = s.ring.as_ref()?;
        let slot = ring.latest()?;
        let (w, h, stride) = (ring.width(), ring.height(), ring.stride());
        let src = ring.slot(slot);
        let row = w as usize * 4;
        let mut pixels = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            pixels.extend_from_slice(&src[y * stride..y * stride + row]);
        }
        Some(Frame { width: w, height: h, pixels: Arc::new(pixels), sequence: s.seq })
    }

    /// A PNG of the surface's latest frame, if it has one.
    pub fn png(&self, key: &SurfaceKey) -> Option<Vec<u8>> {
        self.frame(key).map(|f| f.png())
    }

    pub fn info(&self, key: &SurfaceKey) -> Option<Info> {
        self.inner.lock().unwrap().map.get(key)?.info()
    }

    /// Wait up to `timeout` for anything about `key` newer than `seen`:
    /// an end, a resize, a frame, a cursor change, in that order.
    pub fn wait_newer(&self, key: &SurfaceKey, seen: &Seen, timeout: Duration) -> Ev {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(s) = inner.map.get(key) {
                if let Some(err) = &s.ended {
                    return Ev::End(err.clone());
                }
                let generation = s.ring.as_ref().map_or(0, Ring::generation);
                if generation != seen.generation {
                    return Ev::Resize(s.info());
                }
                if let Some(ring) = &s.ring
                    && s.seq > seen.frame
                    && let Some(slot) = ring.latest()
                {
                    let full = Rect::new(0, 0, ring.width(), ring.height());
                    let (damage, is_full) = match s.damage_since(seen.frame) {
                        Some(d) if d != full => (d, false),
                        _ => (full, true),
                    };
                    return Ev::Frame { slot: slot as u32, seq: s.seq, damage, full: is_full };
                }
                if s.ring.is_some() && s.cursor_seq > seen.cursor {
                    return Ev::Cursor { seq: s.cursor_seq };
                }
            } else if seen.generation != 0 {
                return Ev::Resize(None);
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return Ev::Timeout };
            inner = self.changed.wait_timeout(inner, left).unwrap().0;
        }
    }

    /// Move the cursor (the hot spot) on `key`, and change its image when
    /// `image` is Some (Some(None) hides it).
    pub fn set_cursor(&self, key: &SurfaceKey, x: i32, y: i32, image: Option<Option<&CursorImage>>) {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;
        inner.sequence += 1;
        let seq = inner.sequence;
        let s = inner.map.entry(key.clone()).or_default();
        s.cursor.x = x;
        s.cursor.y = y;
        if let Some(image) = image {
            s.cursor_image = image.cloned();
            let i = s.cursor_image.as_ref();
            s.cursor.visible = i.is_some();
            s.cursor.width = i.map_or(0, |i| i.width);
            s.cursor.height = i.map_or(0, |i| i.height);
            s.cursor.hot_x = i.map_or(0, |i| i.hot_x);
            s.cursor.hot_y = i.map_or(0, |i| i.hot_y);
        }
        s.write_cursor(seq);
        s.cursor_seq = seq;
        self.changed.notify_all();
    }

    /// The surface is off (its scanout was disabled): its ring goes, the
    /// cursor stays for when it's back.
    pub fn remove(&self, key: &SurfaceKey) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.map.get_mut(key) {
            s.ring = None;
            s.damage.clear();
            s.seq = 0;
        }
        drop(inner);
        self.changed.notify_all();
    }

    /// Forget the surface entirely (its source stopped).
    pub fn forget(&self, key: &SurfaceKey) {
        self.inner.lock().unwrap().map.remove(key);
        self.changed.notify_all();
    }

    /// The surface's source failed: viewers get `Ev::End(error)` until
    /// `begin` (a new source starts).
    pub fn end(&self, key: &SurfaceKey, error: &str) {
        let mut inner = self.inner.lock().unwrap();
        let s = inner.map.entry(key.clone()).or_default();
        s.ended = Some(error.to_string());
        s.ring = None;
        drop(inner);
        self.changed.notify_all();
    }

    pub fn begin(&self, key: &SurfaceKey) {
        if let Some(s) = self.inner.lock().unwrap().map.get_mut(key) {
            s.ended = None;
        }
    }

    fn clear_scanouts(&self) {
        self.inner.lock().unwrap().map.retain(|k, _| !matches!(k, SurfaceKey::Scanout(_)));
        self.changed.notify_all();
    }
}

/// One GPU device's scanouts, as surfaces `Scanout(n)` of a registry:
/// `SURFACES` for the VM's device, a registry of its own for a device in a
/// test.
pub struct Frames {
    own: Surfaces,
    shared: Option<&'static Surfaces>,
    cursor: Mutex<Cursor>,
}

impl Default for Frames {
    fn default() -> Self {
        Self::new()
    }
}

impl Frames {
    /// Frames in a registry of their own, in memory.
    pub const fn new() -> Self {
        Self { own: Surfaces::new(), shared: None, cursor: Mutex::new(Cursor { scanout: 0, x: 0, y: 0, image: None }) }
    }

    pub const fn on(surfaces: &'static Surfaces) -> Self {
        Self { own: Surfaces::new(), shared: Some(surfaces), cursor: Mutex::new(Cursor { scanout: 0, x: 0, y: 0, image: None }) }
    }

    pub fn surfaces(&self) -> &Surfaces {
        self.shared.unwrap_or(&self.own)
    }

    /// The latest frame of `scanout`, or None if it's off.
    pub fn latest(&self, scanout: u32) -> Option<Frame> {
        self.surfaces().frame(&SurfaceKey::Scanout(scanout))
    }

    #[cfg(test)]
    pub fn cursor(&self) -> Cursor {
        self.cursor.lock().unwrap().clone()
    }

    /// Show `scanout` at `width` x `height` (SET_SCANOUT), keeping the old
    /// pixels when the size didn't change, and draw `rect` of it with `draw`.
    pub(crate) fn update(&self, scanout: u32, width: u32, height: u32, rect: Rect, draw: impl FnOnce(&mut [u8], usize)) {
        if scanout as usize >= MAX_SCANOUTS {
            return;
        }
        self.surfaces().update(&SurfaceKey::Scanout(scanout), width, height, rect, draw);
    }

    /// The scanout was disabled (SET_SCANOUT with resource 0, or reset).
    pub(crate) fn remove(&self, scanout: u32) {
        self.surfaces().remove(&SurfaceKey::Scanout(scanout));
    }

    pub(crate) fn clear(&self) {
        *self.cursor.lock().unwrap() = Cursor::default();
        self.surfaces().clear_scanouts();
    }

    pub(crate) fn set_cursor(&self, cursor: Cursor) {
        self.surfaces().set_cursor(&SurfaceKey::Scanout(cursor.scanout), cursor.x as i32, cursor.y as i32, Some(cursor.image.as_ref()));
        *self.cursor.lock().unwrap() = cursor;
    }

    pub(crate) fn move_cursor(&self, scanout: u32, x: u32, y: u32) {
        let mut cursor = self.cursor.lock().unwrap();
        cursor.scanout = scanout;
        cursor.x = x;
        cursor.y = y;
        self.surfaces().set_cursor(&SurfaceKey::Scanout(scanout), x as i32, y as i32, None);
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


    fn leak() -> &'static Surfaces {
        Box::leak(Box::new(Surfaces::new()))
    }

    fn session(s: &str) -> SurfaceKey {
        SurfaceKey::Session { machine: "dev".into(), session: s.into() }
    }

    #[test]
    fn frames_keep_the_latest_and_signal_readers() {
        let frames = Frames::new();
        assert!(frames.latest(0).is_none());
        frames.update(0, 4, 2, Rect::new(0, 0, 4, 2), |px, stride| {
            assert_eq!(stride, 256, "ring rows are 256-byte aligned");
            px.fill(7);
        });
        let first = frames.latest(0).unwrap();
        assert_eq!((first.width, first.height, first.pixels.len()), (4, 2, 32));
        // A reader holding the old frame keeps its pixels.
        frames.update(0, 4, 2, Rect::new(0, 0, 1, 1), |px, _| px[0] = 9);
        let second = frames.latest(0).unwrap();
        assert_eq!(first.pixels[0], 7);
        assert_eq!((second.pixels[0], second.pixels[4]), (9, 7));
        assert!(second.sequence > first.sequence);
        // A size change starts a fresh frame.
        frames.update(0, 2, 2, Rect::default(), |_, _| unreachable!());
        assert_eq!(frames.latest(0).unwrap().pixels.len(), 16);
        frames.remove(0);
        assert!(frames.latest(0).is_none());
        frames.update(99, 1, 1, Rect::new(0, 0, 1, 1), |_, _| unreachable!());
    }

    #[test]
    fn waiting_wakes_on_update() {
        let s = leak();
        let t = std::thread::spawn(move || s.wait_newer(&SurfaceKey::Scanout(1), &Seen::default(), Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        s.update(&SurfaceKey::Scanout(1), 1, 1, Rect::new(0, 0, 1, 1), |px, _| px[..4].fill(1));
        assert!(matches!(t.join().unwrap(), Ev::Resize(Some(Info { width: 1, height: 1, .. }))));
    }

    #[test]
    fn display_is_scanouts_of_the_registry() {
        assert!(std::ptr::eq(DISPLAY.surfaces(), &SURFACES));
        let s = leak();
        let frames = Frames::on(s);
        frames.update(0, 8, 8, Rect::new(0, 0, 8, 8), |px, _| px.fill(3));
        let info = s.info(&SurfaceKey::Scanout(0)).unwrap();
        assert_eq!((info.width, info.height, info.stride, info.path), (8, 8, 256, None));
        assert_eq!(s.frame(&SurfaceKey::Scanout(0)).unwrap().pixels[0], 3);
        assert!(s.png(&SurfaceKey::Scanout(0)).unwrap().starts_with(b"\x89PNG"));
        frames.set_cursor(Cursor { scanout: 0, x: 4, y: 5, image: None });
        frames.move_cursor(0, 6, 7);
        assert_eq!((frames.cursor().x, frames.cursor().y), (6, 7));
        // Sessions in the same registry survive a device reset.
        s.update(&session("a"), 2, 2, Rect::new(0, 0, 2, 2), |_, _| {});
        frames.clear();
        assert!(s.info(&SurfaceKey::Scanout(0)).is_none());
        assert!(s.info(&session("a")).is_some());
    }

    #[test]
    fn keys_are_independent_and_waits_wake_per_key() {
        let s = leak();
        s.update(&session("a"), 4, 4, Rect::new(0, 0, 4, 4), |px, _| px.fill(1));
        s.update(&session("b"), 2, 2, Rect::new(0, 0, 2, 2), |px, _| px.fill(2));
        assert_eq!(s.frame(&session("a")).unwrap().pixels[0], 1);
        assert_eq!(s.frame(&session("b")).unwrap().pixels[0], 2);
        assert_ne!(s.info(&session("a")).unwrap().generation, s.info(&session("b")).unwrap().generation);
        let a = s.info(&session("a")).unwrap();
        let seen = Seen { generation: a.generation, frame: s.frame(&session("a")).unwrap().sequence, cursor: u64::MAX };
        let started = Instant::now();
        assert_eq!(s.wait_newer(&session("a"), &seen, Duration::from_millis(30)), Ev::Timeout);
        assert!(started.elapsed() >= Duration::from_millis(30));
        // A frame of b doesn't wake a viewer of a; a frame of a does.
        let t = std::thread::spawn(move || {
            let started = Instant::now();
            (s.wait_newer(&session("a"), &seen, Duration::from_secs(5)), started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(20));
        s.update(&session("b"), 2, 2, Rect::new(0, 0, 1, 1), |_, _| {});
        std::thread::sleep(Duration::from_millis(80));
        s.update(&session("a"), 4, 4, Rect::new(1, 1, 1, 1), |_, _| {});
        let (ev, waited) = t.join().unwrap();
        assert!(matches!(ev, Ev::Frame { damage: Rect { x: 1, y: 1, width: 1, height: 1 }, full: false, .. }), "{ev:?}");
        assert!(waited >= Duration::from_millis(100));
        s.forget(&session("a"));
        assert_eq!(s.wait_newer(&session("a"), &seen, Duration::ZERO), Ev::Resize(None));
    }

    #[test]
    fn skipped_frames_add_up_their_damage() {
        let s = leak();
        let k = session("d");
        s.update(&k, 100, 100, Rect::new(0, 0, 100, 100), |_, _| {});
        let info = s.info(&k).unwrap();
        let first = s.frame(&k).unwrap().sequence;
        let seen = Seen { generation: info.generation, frame: first, cursor: u64::MAX };
        s.update(&k, 100, 100, Rect::new(10, 10, 5, 5), |_, _| {});
        s.update(&k, 100, 100, Rect::new(50, 20, 10, 10), |_, _| {});
        match s.wait_newer(&k, &seen, Duration::ZERO) {
            Ev::Frame { damage, full, .. } => assert_eq!((damage, full), (Rect::new(10, 10, 50, 20), false)),
            ev => panic!("{ev:?}"),
        }
        // 16 frames back is still in the history; 17 isn't.
        for _ in 0..14 {
            s.update(&k, 100, 100, Rect::new(0, 0, 1, 1), |_, _| {});
        }
        assert!(matches!(s.wait_newer(&k, &seen, Duration::ZERO), Ev::Frame { full: false, .. }));
        s.update(&k, 100, 100, Rect::new(0, 0, 1, 1), |_, _| {});
        assert!(matches!(s.wait_newer(&k, &seen, Duration::ZERO), Ev::Frame { full: true, damage: Rect { width: 100, height: 100, .. }, .. }));
        // A new size: a resize first, then a full frame.
        s.update(&k, 50, 50, Rect::new(0, 0, 1, 1), |_, _| {});
        let Ev::Resize(Some(info2)) = s.wait_newer(&k, &seen, Duration::ZERO) else { panic!() };
        assert_ne!(info2.generation, info.generation);
        let latest = s.frame(&k).unwrap().sequence;
        let after = Seen { generation: info2.generation, frame: latest - 1, ..seen };
        assert!(matches!(s.wait_newer(&k, &after, Duration::ZERO), Ev::Frame { full: true, .. }));
    }

    #[test]
    fn slots_keep_undamaged_pixels_across_rotation() {
        let s = leak();
        let k = SurfaceKey::Scanout(0);
        let put = |x: u32, v: u8| s.update(&k, 8, 1, Rect::new(x, 0, 1, 1), |px, _| px[x as usize * 4] = v);
        s.update(&k, 8, 1, Rect::new(0, 0, 8, 1), |px, _| px.fill(1));
        for (i, x) in [3u32, 5, 7, 0, 3].iter().enumerate() {
            put(*x, 10 + i as u8);
        }
        let px: Vec<u8> = s.frame(&k).unwrap().pixels.iter().step_by(4).copied().collect();
        assert_eq!(px, [13, 1, 1, 14, 1, 11, 1, 12]);
    }

    #[test]
    fn cursor_changes_are_events() {
        let s = leak();
        let k = SurfaceKey::Scanout(0);
        let image = CursorImage { width: 2, height: 2, hot_x: 1, hot_y: 1, pixels: Arc::new(vec![0xab; 16]) };
        // Set before the surface exists, it shows up with it.
        s.set_cursor(&k, 3, 4, Some(Some(&image)));
        s.update(&k, 16, 16, Rect::new(0, 0, 16, 16), |_, _| {});
        let info = s.info(&k).unwrap();
        let frame = s.frame(&k).unwrap().sequence;
        let Ev::Cursor { seq } = s.wait_newer(&k, &Seen { generation: info.generation, frame, cursor: 0 }, Duration::ZERO) else { panic!() };
        s.set_cursor(&k, 8, 9, None);
        let Ev::Cursor { seq: moved } = s.wait_newer(&k, &Seen { generation: info.generation, frame, cursor: seq }, Duration::ZERO) else { panic!() };
        assert!(moved > seq);
        let inner = s.inner.lock().unwrap();
        let ring = inner.map[&k].ring.as_ref().unwrap();
        let (state, at) = ring.cursor().unwrap();
        assert_eq!((state.x, state.y, state.hot_x, state.width, state.visible, at), (8, 9, 1, 2, true, moved));
        assert_eq!(ring.cursor_image()[0], 0xab);
    }

    #[test]
    fn rings_are_files_and_a_resize_replaces_them() {
        let dir = std::env::temp_dir().join(format!("berth-surfaces-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("scanout-0.1.ring"), b"stale").unwrap();
        let s = leak();
        s.set_dir(&dir).unwrap();
        assert!(!dir.join("scanout-0.1.ring").exists(), "a previous daemon's rings go");
        let k = session("r");
        s.update(&k, 640, 480, Rect::new(0, 0, 640, 480), |px, _| px.fill(5));
        let old = s.info(&k).unwrap().path.unwrap();
        assert!(old.file_name().unwrap().to_string_lossy().starts_with("session-dev-r."));
        let read = ring::Ring::open_read(&old).unwrap();
        let mut out = Vec::new();
        assert!(read.read_slot(read.latest().unwrap(), &mut out).is_some());
        assert_eq!(out[0], 5);
        s.update(&k, 1920, 1080, Rect::new(0, 0, 1, 1), |_, _| {});
        let new = s.info(&k).unwrap().path.unwrap();
        assert_ne!(new, old);
        assert!(!old.exists() && new.exists());
        s.forget(&k);
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
