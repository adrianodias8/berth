//! A surface's pixels in shared memory: one file per surface and size
//! (`<key>.<gen>.ring` in ~/.berth/run/surfaces), mapped by the daemon to
//! write and by Berth.app to read, so a frame reaches the app without a
//! copy through a socket. The layout is docs/display-protocol.md's, which
//! the app (SurfaceStream.swift) reads byte for byte:
//!
//! - a 16 KiB header: magic, version, gen, size, stride, format, the three
//!   slots' offsets and seqlocks, the latest slot, the cursor;
//! - three slots of BGRA pixels, rows `stride` bytes apart (a multiple of
//!   256, which Metal wants for a linear texture), each slot 16 KiB-aligned;
//! - the cursor image, 256x256 BGRA.
//!
//! A slot's seqlock is `2 * seq` once frame `seq` is in it and odd while a
//! frame is being written, so a reader that sees the same even value before
//! and after copying knows its copy isn't torn. The writer always writes
//! the slot after the latest, so the latest stays readable while it does.
//! A resize is a new file (a new gen), never a remap of the old one.
//!
//! Only frames.rs writes rings (under its lock); `open_read` is for tests,
//! `berth gui watch` and anything else that reads one like the app does.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use crate::gpu::protocol::Rect;

pub const MAGIC: &[u8; 8] = b"BRTHSURF";
pub const VERSION: u32 = 1;
pub const HEADER_BYTES: usize = 16384;
pub const SLOTS: usize = 3;
/// B, G, R, A bytes in memory (Metal's bgra8Unorm).
pub const FORMAT_BGRA8: u32 = 1;
/// `latest` before the first frame.
pub const NO_SLOT: u32 = u32::MAX;
/// The cursor image's side, in pixels; its rows are `CURSOR_SIDE * 4` bytes.
pub const CURSOR_SIDE: u32 = 256;
pub const CURSOR_BYTES: usize = (CURSOR_SIDE * CURSOR_SIDE * 4) as usize;
/// The largest surface side a ring holds (8K is 7680).
pub const MAX_SIDE: u32 = 16384;

/// Byte offsets in the header; every field little-endian. Up to the cursor
/// seq, the fields come in the order the build plan lists them, each
/// naturally aligned, and the app (SurfaceStream.swift's RingLayout) reads
/// exactly these. The sizes after them only let `open_read` check a ring
/// against its file; the app works them out from width and height.
pub mod off {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 8;
    /// `u32`.
    pub const GEN: usize = 12;
    pub const WIDTH: usize = 16;
    pub const HEIGHT: usize = 20;
    pub const STRIDE: usize = 24;
    pub const FORMAT: usize = 28;
    pub const SLOTS: usize = 32;
    /// `u64[3]`.
    pub const SLOT_OFFSET: usize = 40;
    /// `u64[3]`, the seqlocks.
    pub const SLOT_SEQ: usize = 64;
    pub const LATEST: usize = 88;
    pub const CURSOR_X: usize = 96;
    pub const CURSOR_Y: usize = 100;
    pub const CURSOR_HOT_X: usize = 104;
    pub const CURSOR_HOT_Y: usize = 108;
    pub const CURSOR_WIDTH: usize = 112;
    pub const CURSOR_HEIGHT: usize = 116;
    pub const CURSOR_VISIBLE: usize = 120;
    /// The cursor's seqlock, the same rule as a slot's.
    pub const CURSOR_SEQ: usize = 128;
    pub const HEADER_BYTES: usize = 136;
    pub const SLOT_BYTES: usize = 140;
    /// `u64`.
    pub const CURSOR_OFFSET: usize = 144;
}

pub const fn align_up(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

/// Where everything is in a ring of one size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub slot_bytes: usize,
    pub slot_offset: [usize; SLOTS],
    pub cursor_offset: usize,
    pub len: usize,
}

impl Layout {
    /// None for an empty or oversized surface.
    pub fn new(width: u32, height: u32) -> Option<Layout> {
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return None;
        }
        let stride = align_up(width as usize * 4, 256);
        let slot_bytes = align_up(stride * height as usize, 16384);
        let slot_offset = [HEADER_BYTES, HEADER_BYTES + slot_bytes, HEADER_BYTES + 2 * slot_bytes];
        let cursor_offset = HEADER_BYTES + SLOTS * slot_bytes;
        Some(Layout { width, height, stride, slot_bytes, slot_offset, cursor_offset, len: cursor_offset + CURSOR_BYTES })
    }
}

/// The cursor as the header has it. `x`, `y` is where the hot spot is on
/// the surface: the image's top left goes at `x - hot_x`, `y - hot_y`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorState {
    pub x: i32,
    pub y: i32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
}

struct Map {
    ptr: *mut u8,
    len: usize,
}

// The mapping is plain memory; who may write it is Ring's business.
unsafe impl Send for Map {}
unsafe impl Sync for Map {}

impl Drop for Map {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

fn map(fd: Option<&File>, len: usize, writable: bool) -> io::Result<Map> {
    let prot = if writable { libc::PROT_READ | libc::PROT_WRITE } else { libc::PROT_READ };
    let (flags, fd) = match fd {
        Some(f) => (libc::MAP_SHARED, f.as_raw_fd()),
        None => (libc::MAP_PRIVATE | libc::MAP_ANON, -1),
    };
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, 0) };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(Map { ptr: ptr.cast(), len })
}

pub struct Ring {
    map: Map,
    layout: Layout,
    generation: u64,
    /// The file, removed on drop by the ring that created it. None for an
    /// anonymous ring (a GPU device's own frames in tests).
    path: Option<PathBuf>,
    writable: bool,
}

impl Ring {
    /// A new ring for a `width` x `height` surface: `<dir>/<stem>.<gen>.ring`
    /// (mode 0600; a stale file of that name is replaced), or anonymous
    /// memory without a `dir`.
    pub fn create(dir: Option<&Path>, stem: &str, generation: u64, width: u32, height: u32) -> io::Result<Ring> {
        let layout = Layout::new(width, height).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad surface size {width}x{height}")))?;
        let (map, path) = match dir {
            Some(dir) => {
                use std::os::unix::fs::OpenOptionsExt;
                let path = dir.join(format!("{stem}.{generation}.ring"));
                let _ = std::fs::remove_file(&path);
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)?;
                let mapped = file.set_len(layout.len as u64).and_then(|()| map(Some(&file), layout.len, true));
                match mapped {
                    Ok(m) => (m, Some(path)),
                    Err(err) => {
                        let _ = std::fs::remove_file(&path);
                        return Err(err);
                    }
                }
            }
            None => (map(None, layout.len, true)?, None),
        };
        let ring = Ring { map, layout, generation, path, writable: true };
        ring.write_header();
        Ok(ring)
    }

    /// Map an existing ring to read, checking its magic, version and
    /// layout against its size.
    pub fn open_read(path: &Path) -> io::Result<Ring> {
        let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", path.display()));
        let file = File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len < HEADER_BYTES {
            return Err(bad("too short for a ring"));
        }
        let map = map(Some(&file), len, false)?;
        let bytes = unsafe { std::slice::from_raw_parts(map.ptr, HEADER_BYTES) };
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        if &bytes[..8] != MAGIC {
            return Err(bad("not a berth surface ring"));
        }
        if u32_at(off::VERSION) != VERSION {
            return Err(bad(&format!("ring version {}, not {VERSION}", u32_at(off::VERSION))));
        }
        let layout = Layout::new(u32_at(off::WIDTH), u32_at(off::HEIGHT)).ok_or_else(|| bad("bad size"))?;
        let offsets = [0, 1, 2].map(|i| u64_at(off::SLOT_OFFSET + 8 * i) as usize);
        if u32_at(off::HEADER_BYTES) as usize != HEADER_BYTES
            || u32_at(off::STRIDE) as usize != layout.stride
            || u32_at(off::FORMAT) != FORMAT_BGRA8
            || u32_at(off::SLOTS) as usize != SLOTS
            || u32_at(off::SLOT_BYTES) as usize != layout.slot_bytes
            || offsets != layout.slot_offset
            || u64_at(off::CURSOR_OFFSET) as usize != layout.cursor_offset
            || len < layout.len
        {
            return Err(bad("layout doesn't match its size"));
        }
        Ok(Ring { map, layout, generation: u32_at(off::GEN) as u64, path: None, writable: false })
    }

    fn write_header(&self) {
        let l = &self.layout;
        let h = unsafe { std::slice::from_raw_parts_mut(self.map.ptr, HEADER_BYTES) };
        let mut put = |o: usize, b: &[u8]| h[o..o + b.len()].copy_from_slice(b);
        put(off::MAGIC, MAGIC);
        put(off::VERSION, &VERSION.to_le_bytes());
        put(off::HEADER_BYTES, &(HEADER_BYTES as u32).to_le_bytes());
        put(off::GEN, &(self.generation as u32).to_le_bytes());
        put(off::WIDTH, &l.width.to_le_bytes());
        put(off::HEIGHT, &l.height.to_le_bytes());
        put(off::STRIDE, &(l.stride as u32).to_le_bytes());
        put(off::FORMAT, &FORMAT_BGRA8.to_le_bytes());
        put(off::SLOTS, &(SLOTS as u32).to_le_bytes());
        put(off::SLOT_BYTES, &(l.slot_bytes as u32).to_le_bytes());
        for (i, o) in l.slot_offset.iter().enumerate() {
            put(off::SLOT_OFFSET + 8 * i, &(*o as u64).to_le_bytes());
        }
        put(off::CURSOR_OFFSET, &(l.cursor_offset as u64).to_le_bytes());
        // Seqlocks start at 0 (nothing yet), as the new file already is.
        self.u32_at(off::LATEST).store(NO_SLOT, Ordering::Release);
    }

    fn u32_at(&self, o: usize) -> &AtomicU32 {
        debug_assert!(o.is_multiple_of(4) && o < HEADER_BYTES);
        unsafe { &*(self.map.ptr.add(o) as *const AtomicU32) }
    }

    fn u64_at(&self, o: usize) -> &AtomicU64 {
        debug_assert!(o.is_multiple_of(8) && o < HEADER_BYTES);
        unsafe { &*(self.map.ptr.add(o) as *const AtomicU64) }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn width(&self) -> u32 {
        self.layout.width
    }
    pub fn height(&self) -> u32 {
        self.layout.height
    }
    pub fn stride(&self) -> usize {
        self.layout.stride
    }

    /// The slot holding the newest frame, if there is one.
    pub fn latest(&self) -> Option<usize> {
        let i = self.u32_at(off::LATEST).load(Ordering::Acquire);
        ((i as usize) < SLOTS).then_some(i as usize)
    }

    pub fn slot_seq(&self, slot: usize) -> u64 {
        self.u64_at(off::SLOT_SEQ + 8 * slot).load(Ordering::Acquire)
    }

    /// The frame a slot holds (0: none yet); odd seqlocks count as the
    /// frame being written.
    pub fn slot_frame(&self, slot: usize) -> u64 {
        self.slot_seq(slot).div_ceil(2)
    }

    /// Where the next frame goes: after the latest, so the latest stays
    /// whole for readers.
    pub fn next_slot(&self) -> usize {
        self.latest().map_or(0, |i| (i + 1) % SLOTS)
    }

    fn slot_ptr(&self, slot: usize) -> *mut u8 {
        unsafe { self.map.ptr.add(self.layout.slot_offset[slot]) }
    }

    /// A slot's bytes, as they are right now (racy while it's written:
    /// readers outside the daemon use `read_slot`).
    pub fn slot(&self, slot: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.slot_ptr(slot), self.layout.stride * self.layout.height as usize) }
    }

    /// Write frame `seq` (>= 1) into the next slot and make it the latest:
    /// first `copy` (the area that changed since that slot's own frame)
    /// from the latest slot, then `draw` with the slot's bytes and stride.
    pub fn write(&mut self, seq: u64, copy: Option<Rect>, draw: impl FnOnce(&mut [u8], usize)) -> usize {
        assert!(self.writable && seq >= 1);
        let slot = self.next_slot();
        let from = self.latest();
        let lock = self.u64_at(off::SLOT_SEQ + 8 * slot);
        lock.store(2 * seq - 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let stride = self.layout.stride;
        let len = stride * self.layout.height as usize;
        let dst = unsafe { std::slice::from_raw_parts_mut(self.slot_ptr(slot), len) };
        if let (Some(from), Some(r)) = (from, copy.and_then(|r| r.intersect(&Rect::new(0, 0, self.layout.width, self.layout.height))))
            && from != slot
        {
            let src = unsafe { std::slice::from_raw_parts(self.slot_ptr(from), len) };
            let n = r.width as usize * 4;
            for row in r.y as usize..(r.y + r.height) as usize {
                let o = row * stride + r.x as usize * 4;
                dst[o..o + n].copy_from_slice(&src[o..o + n]);
            }
        }
        draw(dst, stride);
        lock.store(2 * seq, Ordering::Release);
        self.u32_at(off::LATEST).store(slot as u32, Ordering::Release);
        slot
    }

    /// `write` with `rect`'s pixels as packed rows (`rect.width * 4` bytes each).
    #[cfg(test)]
    pub fn write_rect(&mut self, seq: u64, copy: Option<Rect>, rect: Rect, rows: &[u8]) -> usize {
        self.write(seq, copy, |px, stride| blit(px, stride, rect, rows))
    }

    /// Copy slot `slot` into `out` (packed rows of `stride`) and return the
    /// frame it held, or None if it was being written or changed meanwhile.
    pub fn read_slot(&self, slot: usize, out: &mut Vec<u8>) -> Option<u64> {
        let lock = self.u64_at(off::SLOT_SEQ + 8 * slot);
        let before = lock.load(Ordering::Acquire);
        if before == 0 || before % 2 == 1 {
            return None;
        }
        let len = self.layout.stride * self.layout.height as usize;
        out.resize(len, 0);
        unsafe { std::ptr::copy_nonoverlapping(self.slot_ptr(slot), out.as_mut_ptr(), len) };
        fence(Ordering::Acquire);
        (lock.load(Ordering::Relaxed) == before).then_some(before / 2)
    }

    /// Change the cursor: its position always, its image when `image` is
    /// given (packed rows, at most 256x256).
    pub fn write_cursor(&mut self, seq: u64, state: CursorState, image: Option<&[u8]>) {
        assert!(self.writable && seq >= 1);
        let lock = self.u64_at(off::CURSOR_SEQ);
        lock.store(2 * seq - 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let (w, h) = (state.width.min(CURSOR_SIDE), state.height.min(CURSOR_SIDE));
        self.u32_at(off::CURSOR_X).store(state.x as u32, Ordering::Relaxed);
        self.u32_at(off::CURSOR_Y).store(state.y as u32, Ordering::Relaxed);
        self.u32_at(off::CURSOR_HOT_X).store(state.hot_x, Ordering::Relaxed);
        self.u32_at(off::CURSOR_HOT_Y).store(state.hot_y, Ordering::Relaxed);
        self.u32_at(off::CURSOR_WIDTH).store(w, Ordering::Relaxed);
        self.u32_at(off::CURSOR_HEIGHT).store(h, Ordering::Relaxed);
        self.u32_at(off::CURSOR_VISIBLE).store(state.visible as u32, Ordering::Relaxed);
        if let Some(image) = image {
            let dst = unsafe { std::slice::from_raw_parts_mut(self.map.ptr.add(self.layout.cursor_offset), CURSOR_BYTES) };
            dst.fill(0);
            let src_row = state.width as usize * 4;
            for y in 0..h as usize {
                let n = w as usize * 4;
                if let Some(src) = image.get(y * src_row..y * src_row + n) {
                    dst[y * CURSOR_SIDE as usize * 4..][..n].copy_from_slice(src);
                }
            }
        }
        lock.store(2 * seq, Ordering::Release);
    }

    /// The cursor fields and the cursor change they belong to (None while
    /// it's being written). The app reads these fields itself.
    #[cfg(test)]
    pub fn cursor(&self) -> Option<(CursorState, u64)> {
        let lock = self.u64_at(off::CURSOR_SEQ);
        let before = lock.load(Ordering::Acquire);
        if before % 2 == 1 {
            return None;
        }
        let r = |o| self.u32_at(o).load(Ordering::Relaxed);
        let state = CursorState {
            x: r(off::CURSOR_X) as i32,
            y: r(off::CURSOR_Y) as i32,
            hot_x: r(off::CURSOR_HOT_X),
            hot_y: r(off::CURSOR_HOT_Y),
            width: r(off::CURSOR_WIDTH),
            height: r(off::CURSOR_HEIGHT),
            visible: r(off::CURSOR_VISIBLE) != 0,
        };
        fence(Ordering::Acquire);
        (lock.load(Ordering::Relaxed) == before).then_some((state, before / 2))
    }

    #[cfg(test)]
    pub fn cursor_image(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.map.ptr.add(self.layout.cursor_offset), CURSOR_BYTES) }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        if self.writable
            && let Some(path) = &self.path
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Copy packed rows of `rect` into an image of `stride`.
pub fn blit(dst: &mut [u8], stride: usize, rect: Rect, rows: &[u8]) {
    let n = rect.width as usize * 4;
    for (r, src) in rows.chunks_exact(n.max(1)).take(rect.height as usize).enumerate() {
        let o = (rect.y as usize + r) * stride + rect.x as usize * 4;
        dst[o..o + n].copy_from_slice(src);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("berth-ring-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn layout_is_aligned() {
        for (w, h) in [(640, 480), (1366, 768), (1920, 1080), (7680, 4320)] {
            let l = Layout::new(w, h).unwrap();
            assert_eq!(l.stride % 256, 0, "{w}");
            assert!(l.stride >= w as usize * 4);
            assert!(l.slot_offset.iter().all(|o| o % 16384 == 0), "{w}");
            assert_eq!(l.slot_offset[0], HEADER_BYTES);
            assert_eq!(l.cursor_offset % 16384, 0);
            assert_eq!(l.len, l.cursor_offset + 256 * 256 * 4);
        }
        assert_eq!(Layout::new(1920, 1080).unwrap().stride, 7680);
        assert_eq!(Layout::new(1366, 768).unwrap().stride, 5632);
        assert!(Layout::new(0, 10).is_none() && Layout::new(MAX_SIDE + 1, 1).is_none());
    }

    #[test]
    fn header_reads_back_and_bad_files_are_refused() {
        let dir = tmp("header");
        let ring = Ring::create(Some(&dir), "scanout-0", 7, 1366, 768).unwrap();
        let path = ring.path().unwrap().to_path_buf();
        assert_eq!(path, dir.join("scanout-0.7.ring"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], b"BRTHSURF");
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        // The offsets the app's RingLayout reads.
        assert_eq!((u32_at(8), u32_at(12)), (1, 7));
        assert_eq!((u32_at(16), u32_at(20), u32_at(24), u32_at(28), u32_at(32)), (1366, 768, 5632, 1, 3));
        let slot_bytes = 5632 * 768;
        assert_eq!((u64_at(40), u64_at(48), u64_at(56)), (16384, 16384 + slot_bytes, 16384 + 2 * slot_bytes));
        assert_eq!((u64_at(64), u32_at(88)), (0, NO_SLOT));
        assert_eq!((u32_at(136), u32_at(140), u64_at(144)), (16384, slot_bytes as u32, 16384 + 3 * slot_bytes));
        let read = Ring::open_read(&path).unwrap();
        assert_eq!((read.generation(), read.width(), read.height(), read.latest()), (7, 1366, 768, None));

        let mut bad = bytes.clone();
        bad[0] = b'X';
        std::fs::write(dir.join("bad-magic"), &bad).unwrap();
        assert!(Ring::open_read(&dir.join("bad-magic")).is_err());
        let mut bad = bytes.clone();
        bad[off::VERSION] = 2;
        std::fs::write(dir.join("bad-version"), &bad).unwrap();
        assert!(Ring::open_read(&dir.join("bad-version")).err().unwrap().to_string().contains("version 2"));
        std::fs::write(dir.join("short"), &bytes[..HEADER_BYTES + 10]).unwrap();
        assert!(Ring::open_read(&dir.join("short")).is_err());

        drop(read);
        drop(ring);
        assert!(!path.exists(), "the writer removes its file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_rect_keeps_the_rest_and_rotates_slots() {
        let mut ring = Ring::create(None, "t", 1, 8, 4).unwrap();
        let full = vec![1u8; 8 * 4 * 4];
        assert_eq!(ring.write_rect(1, None, Rect::new(0, 0, 8, 4), &full), 0);
        // Frame 2 changes one 2x2 square; its slot had nothing, so all of
        // frame 1 is copied in first.
        assert_eq!(ring.write_rect(2, Some(Rect::new(0, 0, 8, 4)), Rect::new(1, 1, 2, 2), &[9u8; 16]), 1);
        let px = |ring: &Ring, slot: usize, x: usize, y: usize| ring.slot(slot)[y * ring.stride() + x * 4];
        assert_eq!((px(&ring, 1, 0, 0), px(&ring, 1, 1, 1), px(&ring, 1, 2, 2), px(&ring, 1, 3, 3)), (1, 9, 9, 1));
        assert_eq!(px(&ring, 0, 1, 1), 1, "the previous slot is untouched");
        assert_eq!((ring.latest(), ring.slot_frame(1), ring.slot_seq(1)), (Some(1), 2, 4));
        assert_eq!(ring.write_rect(3, Some(Rect::new(0, 0, 8, 4)), Rect::new(7, 3, 1, 1), &[5u8; 4]), 2);
        assert_eq!(ring.write_rect(4, Some(Rect::new(1, 1, 7, 3)), Rect::new(0, 0, 1, 1), &[6u8; 4]), 0);
        assert_eq!((px(&ring, 0, 0, 0), px(&ring, 0, 1, 1), px(&ring, 0, 7, 3), px(&ring, 0, 4, 0)), (6, 9, 5, 1));
        let mut out = Vec::new();
        assert_eq!(ring.read_slot(0, &mut out), Some(4));
        assert_eq!(out.len(), ring.stride() * 4);
    }

    #[test]
    fn cursor_round_trips() {
        let mut ring = Ring::create(None, "t", 1, 64, 64).unwrap();
        assert_eq!(ring.cursor(), Some((CursorState::default(), 0)));
        let state = CursorState { x: 10, y: -3, hot_x: 1, hot_y: 2, width: 2, height: 2, visible: true };
        ring.write_cursor(5, state, Some(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]));
        assert_eq!(ring.cursor(), Some((state, 5)));
        assert_eq!(&ring.cursor_image()[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&ring.cursor_image()[1024..1032], &[9, 10, 11, 12, 13, 14, 15, 16]);
    }

    /// A reader in another thread, mapping the file as the app does, never
    /// accepts a slot whose pixels aren't all from one frame.
    #[test]
    fn seqlock_never_shows_a_torn_slot() {
        let dir = tmp("seqlock");
        let (w, h) = (256u32, 64u32);
        let mut ring = Ring::create(Some(&dir), "s", 1, w, h).unwrap();
        let path = ring.path().unwrap().to_path_buf();
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let done = done.clone();
            std::thread::spawn(move || {
                let ring = Ring::open_read(&path).unwrap();
                let (mut accepted, mut torn, mut out) = (0u32, 0u32, Vec::new());
                while !done.load(Ordering::Acquire) {
                    let Some(slot) = ring.latest() else { continue };
                    match ring.read_slot(slot, &mut out) {
                        Some(seq) => {
                            let want = (seq % 251) as u8;
                            assert!(out.iter().all(|&b| b == want), "slot {slot} frame {seq} is torn");
                            accepted += 1;
                        }
                        None => torn += 1,
                    }
                }
                (accepted, torn)
            })
        };
        for seq in 1..=10_000u64 {
            let v = (seq % 251) as u8;
            ring.write(seq, None, |px, _| px.fill(v));
        }
        done.store(true, Ordering::Release);
        let (accepted, _torn) = reader.join().unwrap();
        assert!(accepted > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
