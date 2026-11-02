//! The virtio-gpu command server: the resource table and every 2D command,
//! independent of Virtualization.framework so it's tested here with fake
//! guest memory. device.rs feeds it requests off the virtqueues; it answers
//! with response bytes and publishes what the guest shows to frames.rs.
//!
//! Everything a request carries is untrusted. Resource sizes are capped
//! (`MAX_DIMENSION`, and the host memory all resources may take), every
//! rectangle and offset is checked against the resource and its backing
//! before a byte is copied (in u64, so hostile values can't wrap), and a
//! failed command changes nothing.
//!
//! 3D, contexts and blob resources answer ERR_UNSPEC: the device doesn't
//! offer VIRGL, CONTEXT_INIT or RESOURCE_BLOB (device.rs), so a conforming
//! guest never sends them. virgl.rs is where they'll be served.

use std::collections::HashMap;

use super::frames::{self, CursorImage, Frames};
use super::protocol::{Command, CtrlHdr, CursorCmd, Format, MemEntry, ParseError, Rect, Request, Response, cmd, edid};

/// Largest width or height of a 2D resource (Linux's virtio_gpu caps its
/// mode config at 16384 too).
pub const MAX_DIMENSION: u32 = 16384;
/// Most guest memory one resource's backing may span.
const MAX_BACKING_BYTES: u64 = 4 << 30;
/// Cursor images are 64x64; anything over this isn't a cursor.
const MAX_CURSOR: u32 = 256;
/// Errors logged before going quiet (a broken guest could send millions).
const LOGGED_ERRORS: u32 = 50;

/// Guest RAM as the device sees it.
pub trait GuestMemory {
    /// Map `len` bytes of guest physical memory at `addr`, or None if that
    /// isn't guest RAM.
    fn map(&self, addr: u64, len: usize) -> Option<Box<dyn Mapping>>;
}

/// A mapped piece of guest RAM. The guest can change it at any time, so
/// it's only ever read by copying, never through a Rust reference.
pub trait Mapping {
    fn as_ptr(&self) -> *const u8;
    fn len(&self) -> usize;
}

/// A resource's guest memory: scatter-gather pieces, mapped once at attach.
struct Backing {
    entries: Vec<MemEntry>,
    /// Each mapping with the length of guest memory it stands for (a
    /// mapping may be longer than asked).
    pieces: Vec<(Box<dyn Mapping>, usize)>,
    /// Where each piece starts in the backing, for the binary search.
    starts: Vec<u64>,
    total: u64,
}

impl Backing {
    /// Map `entries`, joining physically contiguous ones first: Linux's
    /// pages are often adjacent, and one mapping beats thousands.
    fn map(entries: Vec<MemEntry>, mem: &dyn GuestMemory) -> Option<Self> {
        // (address, length, entries in it), in the guest's order.
        let mut runs: Vec<(u64, u64, usize)> = Vec::new();
        for e in &entries {
            match runs.last_mut() {
                Some((addr, len, n)) if addr.checked_add(*len) == Some(e.addr) => {
                    *len += e.length as u64;
                    *n += 1;
                }
                _ => runs.push((e.addr, e.length as u64, 1)),
            }
        }
        let piece = |addr: u64, len: u64| -> Option<(Box<dyn Mapping>, usize)> {
            let len = usize::try_from(len).ok()?;
            let m = mem.map(addr, len)?;
            (m.len() >= len).then_some((m, len))
        };
        let mut pieces = Vec::with_capacity(runs.len());
        let mut first = 0;
        for &(addr, len, n) in &runs {
            match piece(addr, len) {
                Some(p) => pieces.push(p),
                // A run across two RAM regions: map its entries one by one.
                None => {
                    for e in &entries[first..first + n] {
                        pieces.push(piece(e.addr, e.length as u64)?);
                    }
                }
            }
            first += n;
        }
        let mut starts = Vec::with_capacity(pieces.len());
        let mut total = 0u64;
        for (_, len) in &pieces {
            starts.push(total);
            total += *len as u64;
        }
        Some(Self { entries, pieces, starts, total })
    }

    /// Copy `out.len()` bytes at `offset` in the backing. False (and
    /// nothing copied) if that's past its end.
    fn read(&self, offset: u64, out: &mut [u8]) -> bool {
        let Some(end) = offset.checked_add(out.len() as u64) else { return false };
        if end > self.total {
            return false;
        }
        if out.is_empty() {
            return true;
        }
        let mut i = self.starts.partition_point(|&s| s <= offset) - 1;
        let (mut at, mut done) = ((offset - self.starts[i]) as usize, 0usize);
        while done < out.len() {
            let (map, len) = &self.pieces[i];
            let n = (len - at).min(out.len() - done);
            // SAFETY: `at + n` is within the mapping (checked against its
            // length when mapped); the guest may write it concurrently,
            // which can only tear pixels, as on real hardware.
            unsafe { std::ptr::copy_nonoverlapping(map.as_ptr().add(at), out[done..].as_mut_ptr(), n) };
            done += n;
            i += 1;
            at = 0;
        }
        true
    }
}

struct Resource {
    format: Format,
    width: u32,
    height: u32,
    /// The host copy, in the guest's format, rows of `width * 4` bytes.
    pixels: Vec<u8>,
    backing: Option<Backing>,
    /// Backing from a restored snapshot that couldn't be mapped yet:
    /// mapped on first use.
    pending: Option<Vec<MemEntry>>,
    uuid: Option<[u8; 16]>,
}

impl Resource {
    fn stride(&self) -> u64 {
        self.width as u64 * Format::BYTES_PER_PIXEL as u64
    }
    fn bytes(&self) -> u64 {
        self.stride() * self.height as u64
    }
}

/// What a scanout shows: part of a resource.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
struct Binding {
    resource_id: u32,
    rect: Rect,
}

/// A capability set the device offers (from the 3D renderer).
#[derive(Clone, Debug)]
pub struct Capset {
    pub id: u32,
    pub max_version: u32,
    pub data: Vec<u8>,
}

/// The device's state.
pub struct Server {
    /// Mode of every scanout (`num_scanouts` is its length).
    modes: Vec<(u32, u32)>,
    capsets: Vec<Capset>,
    resources: HashMap<u32, Resource>,
    scanouts: Vec<Option<Binding>>,
    cursor: Option<CursorCmd>,
    /// Host bytes all resources take, against `host_limit`.
    host_bytes: u64,
    host_limit: u64,
    frames: &'static Frames,
    errors: u32,
}

type Answer = Result<Response, u32>;

impl Server {
    /// `modes`: the size of each scanout (1 to 16); `host_limit`: bytes all
    /// resources together may take on the Mac.
    pub fn new(modes: Vec<(u32, u32)>, capsets: Vec<Capset>, host_limit: u64, frames: &'static Frames) -> Self {
        assert!((1..=super::protocol::MAX_SCANOUTS).contains(&modes.len()));
        let scanouts = vec![None; modes.len()];
        Self { modes, capsets, resources: HashMap::new(), scanouts, cursor: None, host_bytes: 0, host_limit, frames, errors: 0 }
    }

    pub fn num_scanouts(&self) -> u32 {
        self.modes.len() as u32
    }

    pub fn num_capsets(&self) -> u32 {
        self.capsets.len() as u32
    }

    /// Serve one request; the reply to write back (the cursor queue's
    /// requests have no room for one, and device.rs drops it then).
    pub fn handle(&mut self, request: &[u8], mem: &dyn GuestMemory) -> Vec<u8> {
        let hdr = match CtrlHdr::parse(request) {
            Ok(hdr) => hdr,
            Err(err) => {
                self.log(format_args!("{err}"));
                return Response::Err(cmd::RESP_ERR_UNSPEC).serialize(&CtrlHdr::default());
            }
        };
        let answer = match Request::parse(request) {
            Ok(req) => {
                let name = command_name(&req.cmd);
                let answer = self.execute(req.cmd, mem);
                if let Err(code) = answer {
                    self.log(format_args!("{name}: {}", error_name(code)));
                }
                answer
            }
            Err(err) => {
                self.log(format_args!("{err}"));
                Err(match err {
                    ParseError::TooManyEntries(_) => cmd::RESP_ERR_OUT_OF_MEMORY,
                    _ => cmd::RESP_ERR_UNSPEC,
                })
            }
        };
        answer.unwrap_or_else(Response::Err).serialize(&hdr)
    }

    fn log(&mut self, what: std::fmt::Arguments) {
        if self.errors < LOGGED_ERRORS {
            eprintln!("gpu: {what}");
        } else if self.errors == LOGGED_ERRORS {
            eprintln!("gpu: more errors; not logging them");
        }
        self.errors = self.errors.saturating_add(1);
    }

    fn execute(&mut self, command: Command, mem: &dyn GuestMemory) -> Answer {
        use cmd::*;
        match command {
            Command::GetDisplayInfo => Ok(Response::OkDisplayInfo(
                self.modes
                    .iter()
                    .map(|&(w, h)| super::protocol::DisplayOne { rect: Rect::new(0, 0, w, h), enabled: true, flags: 0 })
                    .collect(),
            )),
            Command::GetEdid { scanout } => {
                let &(w, h) = self.modes.get(scanout as usize).ok_or(RESP_ERR_INVALID_PARAMETER)?;
                Ok(Response::OkEdid(edid(w, h).to_vec()))
            }
            Command::ResourceCreate2d { resource_id, format, width, height } => {
                self.create(resource_id, format, width, height).map(|()| Response::OkNoData)
            }
            Command::ResourceUnref { resource_id } => {
                let res = self.resources.remove(&resource_id).ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                self.host_bytes -= res.bytes();
                for (i, s) in self.scanouts.iter_mut().enumerate() {
                    if s.is_some_and(|b| b.resource_id == resource_id) {
                        *s = None;
                        self.frames.remove(i as u32);
                    }
                }
                Ok(Response::OkNoData)
            }
            Command::ResourceAttachBacking { resource_id, entries } => {
                self.attach(resource_id, entries, mem).map(|()| Response::OkNoData)
            }
            Command::ResourceDetachBacking { resource_id } => {
                let res = self.resources.get_mut(&resource_id).ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                if res.backing.take().is_none() && res.pending.take().is_none() {
                    return Err(RESP_ERR_UNSPEC);
                }
                Ok(Response::OkNoData)
            }
            Command::TransferToHost2d { rect, offset, resource_id } => {
                self.transfer(resource_id, rect, offset, mem).map(|()| Response::OkNoData)
            }
            Command::SetScanout { rect, scanout_id, resource_id } => {
                self.set_scanout(scanout_id, resource_id, rect).map(|()| Response::OkNoData)
            }
            Command::ResourceFlush { rect, resource_id } => {
                let res = self.resources.get(&resource_id).ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                if !rect.fits(res.width, res.height) {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                for (i, b) in self.scanouts.iter().enumerate() {
                    if let Some(b) = b.filter(|b| b.resource_id == resource_id)
                        && let Some(dirty) = rect.intersect(&b.rect)
                    {
                        publish(self.frames, i as u32, b, res, dirty);
                    }
                }
                Ok(Response::OkNoData)
            }
            Command::GetCapsetInfo { capset_index } => {
                let c = self.capsets.get(capset_index as usize).ok_or(RESP_ERR_INVALID_PARAMETER)?;
                Ok(Response::OkCapsetInfo { capset_id: c.id, max_version: c.max_version, max_size: c.data.len() as u32 })
            }
            Command::GetCapset { capset_id, capset_version } => {
                let c = self
                    .capsets
                    .iter()
                    .find(|c| c.id == capset_id && capset_version <= c.max_version)
                    .ok_or(RESP_ERR_INVALID_PARAMETER)?;
                Ok(Response::OkCapset(c.data.clone()))
            }
            Command::ResourceAssignUuid { resource_id } => {
                let res = self.resources.get_mut(&resource_id).ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                Ok(Response::OkResourceUuid(*res.uuid.get_or_insert_with(new_uuid)))
            }
            Command::UpdateCursor(c) => self.update_cursor(c).map(|()| Response::OkNoData),
            Command::MoveCursor(c) => {
                if c.scanout_id >= self.num_scanouts() {
                    return Err(RESP_ERR_INVALID_SCANOUT_ID);
                }
                if let Some(cur) = &mut self.cursor {
                    (cur.scanout_id, cur.x, cur.y) = (c.scanout_id, c.x, c.y);
                }
                self.frames.move_cursor(c.scanout_id, c.x, c.y);
                Ok(Response::OkNoData)
            }
            // Not offered (see the module comment).
            Command::ResourceCreateBlob { .. }
            | Command::SetScanoutBlob { .. }
            | Command::ResourceMapBlob { .. }
            | Command::ResourceUnmapBlob { .. }
            | Command::CtxCreate { .. }
            | Command::CtxDestroy
            | Command::CtxAttachResource { .. }
            | Command::CtxDetachResource { .. }
            | Command::ResourceCreate3d(_)
            | Command::TransferToHost3d(_)
            | Command::TransferFromHost3d(_)
            | Command::Submit3d { .. } => Err(RESP_ERR_UNSPEC),
        }
    }

    fn create(&mut self, id: u32, format: u32, width: u32, height: u32) -> Result<(), u32> {
        if id == 0 || self.resources.contains_key(&id) {
            return Err(cmd::RESP_ERR_INVALID_RESOURCE_ID);
        }
        let format = Format::from_u32(format).ok_or(cmd::RESP_ERR_INVALID_PARAMETER)?;
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        let bytes = width as u64 * height as u64 * Format::BYTES_PER_PIXEL as u64;
        if self.host_bytes + bytes > self.host_limit {
            return Err(cmd::RESP_ERR_OUT_OF_MEMORY);
        }
        self.host_bytes += bytes;
        // Zeroed allocations are lazily committed: untouched rows cost nothing.
        let pixels = vec![0u8; bytes as usize];
        self.resources.insert(id, Resource { format, width, height, pixels, backing: None, pending: None, uuid: None });
        Ok(())
    }

    fn attach(&mut self, id: u32, entries: Vec<MemEntry>, mem: &dyn GuestMemory) -> Result<(), u32> {
        let res = self.resources.get_mut(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if res.backing.is_some() || res.pending.is_some() {
            return Err(cmd::RESP_ERR_UNSPEC);
        }
        let total: u64 = entries.iter().map(|e| e.length as u64).sum();
        if entries.is_empty() || entries.iter().any(|e| e.length == 0) || total > MAX_BACKING_BYTES {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        res.backing = Some(Backing::map(entries, mem).ok_or(cmd::RESP_ERR_UNSPEC)?);
        Ok(())
    }

    /// TRANSFER_TO_HOST_2D: rows of `rect` from the backing into the host
    /// copy. Row `r` of the rectangle is at `offset + r * stride` in the
    /// backing, which has the resource's layout (this is what Linux and
    /// QEMU do).
    fn transfer(&mut self, id: u32, rect: Rect, offset: u64, mem: &dyn GuestMemory) -> Result<(), u32> {
        let res = self.resources.get_mut(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if !rect.fits(res.width, res.height) {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        if res.backing.is_none()
            && let Some(entries) = res.pending.take()
        {
            res.backing = Backing::map(entries.clone(), mem);
            if res.backing.is_none() {
                res.pending = Some(entries);
            }
        }
        let stride = res.stride();
        let backing = res.backing.as_ref().ok_or(cmd::RESP_ERR_UNSPEC)?;
        if rect.is_empty() {
            return Ok(());
        }
        let row = rect.width as u64 * Format::BYTES_PER_PIXEL as u64;
        // The whole transfer is checked before anything is copied.
        let last = (rect.height as u64 - 1).checked_mul(stride).and_then(|o| o.checked_add(offset)).and_then(|o| o.checked_add(row));
        if last.is_none_or(|end| end > backing.total) {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        for r in 0..rect.height as u64 {
            let src = offset + r * stride;
            let dst = ((rect.y as u64 + r) * stride + rect.x as u64 * Format::BYTES_PER_PIXEL as u64) as usize;
            let ok = backing.read(src, &mut res.pixels[dst..dst + row as usize]);
            debug_assert!(ok);
        }
        Ok(())
    }

    fn set_scanout(&mut self, scanout: u32, id: u32, rect: Rect) -> Result<(), u32> {
        let slot = self.scanouts.get_mut(scanout as usize).ok_or(cmd::RESP_ERR_INVALID_SCANOUT_ID)?;
        if id == 0 {
            *slot = None;
            self.frames.remove(scanout);
            return Ok(());
        }
        let res = self.resources.get(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if rect.is_empty() || !rect.fits(res.width, res.height) {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        let binding = Binding { resource_id: id, rect };
        *slot = Some(binding);
        publish(self.frames, scanout, binding, res, rect);
        Ok(())
    }

    fn update_cursor(&mut self, c: CursorCmd) -> Result<(), u32> {
        if c.scanout_id >= self.num_scanouts() {
            return Err(cmd::RESP_ERR_INVALID_SCANOUT_ID);
        }
        let image = if c.resource_id == 0 {
            None
        } else {
            let res = self.resources.get(&c.resource_id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
            if res.width > MAX_CURSOR || res.height > MAX_CURSOR {
                return Err(cmd::RESP_ERR_INVALID_PARAMETER);
            }
            let mut pixels = vec![0u8; res.bytes() as usize];
            convert(&res.pixels, &mut pixels, res.format);
            Some(CursorImage {
                width: res.width,
                height: res.height,
                hot_x: c.hot_x.min(res.width.saturating_sub(1)),
                hot_y: c.hot_y.min(res.height.saturating_sub(1)),
                pixels: pixels.into(),
            })
        };
        self.cursor = Some(c);
        self.frames.set_cursor(frames::Cursor { scanout: c.scanout_id, x: c.x, y: c.y, image });
        Ok(())
    }

    /// The guest reset the device: everything goes.
    pub fn reset(&mut self) {
        self.resources.clear();
        self.scanouts.iter_mut().for_each(|s| *s = None);
        self.cursor = None;
        self.host_bytes = 0;
        self.frames.clear();
    }

    /// The state a VM snapshot needs: resources and where their backing is
    /// in guest memory (which the snapshot holds), scanouts and the cursor.
    /// Host pixels aren't saved; they're read back from the backing.
    pub fn save_state(&self) -> Vec<u8> {
        let mut resources: Vec<SavedResource> = self
            .resources
            .iter()
            .map(|(&id, r)| SavedResource {
                id,
                format: r.format as u32,
                width: r.width,
                height: r.height,
                entries: r.backing.as_ref().map(|b| b.entries.clone()).or_else(|| r.pending.clone()).unwrap_or_default(),
                uuid: r.uuid,
            })
            .collect();
        resources.sort_by_key(|r| r.id);
        let saved = Saved {
            version: 1,
            resources,
            scanouts: self.scanouts.clone(),
            cursor: self.cursor.map(|c| (c.scanout_id, c.x, c.y, c.resource_id, c.hot_x, c.hot_y)),
        };
        serde_json::to_vec(&saved).unwrap()
    }

    /// Rebuild from `save_state`, through the same checks as the guest's
    /// commands. False if the state isn't one we saved.
    pub fn restore_state(&mut self, state: &[u8], mem: &dyn GuestMemory) -> bool {
        let Ok(saved) = serde_json::from_slice::<Saved>(state) else { return false };
        if saved.version != 1 {
            return false;
        }
        self.reset();
        for r in saved.resources {
            if self.create(r.id, r.format, r.width, r.height).is_err() {
                return false;
            }
            let res = self.resources.get_mut(&r.id).unwrap();
            res.uuid = r.uuid;
            if r.entries.is_empty() {
                continue;
            }
            if self.attach(r.id, r.entries.clone(), mem).is_err() {
                self.resources.get_mut(&r.id).unwrap().pending = Some(r.entries);
                continue;
            }
            // The guest's copy is the truth: refill the host's from it.
            let _ = self.transfer(r.id, Rect::new(0, 0, r.width, r.height), 0, mem);
        }
        for (i, b) in saved.scanouts.into_iter().enumerate().take(self.scanouts.len()) {
            if let Some(b) = b {
                let _ = self.set_scanout(i as u32, b.resource_id, b.rect);
            }
        }
        if let Some((scanout_id, x, y, resource_id, hot_x, hot_y)) = saved.cursor {
            let _ = self.update_cursor(CursorCmd { scanout_id, x, y, resource_id, hot_x, hot_y });
        }
        true
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedResource {
    id: u32,
    format: u32,
    width: u32,
    height: u32,
    entries: Vec<MemEntry>,
    uuid: Option<[u8; 16]>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    version: u32,
    resources: Vec<SavedResource>,
    scanouts: Vec<Option<Binding>>,
    cursor: Option<(u32, u32, u32, u32, u32, u32)>,
}

/// Draw `dirty` (resource coordinates, inside the binding's rect) of a
/// scanout's resource into its frame.
fn publish(frames: &Frames, scanout: u32, b: Binding, res: &Resource, dirty: Rect) {
    let into = Rect::new(dirty.x - b.rect.x, dirty.y - b.rect.y, dirty.width, dirty.height);
    let src_stride = res.stride() as usize;
    frames.update(scanout, b.rect.width, b.rect.height, into, |px, stride| {
        let n = dirty.width as usize * 4;
        for r in 0..dirty.height as usize {
            let s = (dirty.y as usize + r) * src_stride + dirty.x as usize * 4;
            let d = (into.y as usize + r) * stride + into.x as usize * 4;
            convert(&res.pixels[s..s + n], &mut px[d..d + n], res.format);
        }
    });
}

/// Pixels in `format` to BGRA, opaque where the format has no alpha.
fn convert(src: &[u8], dst: &mut [u8], format: Format) {
    match format.layout() {
        ([0, 1, 2], Some(3)) => dst.copy_from_slice(src),
        ([b, g, r], a) => {
            for (s, d) in src.as_chunks::<4>().0.iter().zip(dst.as_chunks_mut::<4>().0) {
                d[0] = s[b];
                d[1] = s[g];
                d[2] = s[r];
                d[3] = a.map_or(0xff, |a| s[a]);
            }
        }
    }
}

fn new_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    unsafe { libc::arc4random_buf(u.as_mut_ptr().cast(), u.len()) };
    u[6] = (u[6] & 0x0f) | 0x40; // version 4
    u[8] = (u[8] & 0x3f) | 0x80; // RFC 4122 variant
    u
}

fn command_name(c: &Command) -> &'static str {
    match c {
        Command::GetDisplayInfo => "GET_DISPLAY_INFO",
        Command::ResourceCreate2d { .. } => "RESOURCE_CREATE_2D",
        Command::ResourceUnref { .. } => "RESOURCE_UNREF",
        Command::SetScanout { .. } => "SET_SCANOUT",
        Command::ResourceFlush { .. } => "RESOURCE_FLUSH",
        Command::TransferToHost2d { .. } => "TRANSFER_TO_HOST_2D",
        Command::ResourceAttachBacking { .. } => "RESOURCE_ATTACH_BACKING",
        Command::ResourceDetachBacking { .. } => "RESOURCE_DETACH_BACKING",
        Command::GetCapsetInfo { .. } => "GET_CAPSET_INFO",
        Command::GetCapset { .. } => "GET_CAPSET",
        Command::GetEdid { .. } => "GET_EDID",
        Command::ResourceAssignUuid { .. } => "RESOURCE_ASSIGN_UUID",
        Command::ResourceCreateBlob { .. } => "RESOURCE_CREATE_BLOB",
        Command::SetScanoutBlob { .. } => "SET_SCANOUT_BLOB",
        Command::CtxCreate { .. } => "CTX_CREATE",
        Command::CtxDestroy => "CTX_DESTROY",
        Command::CtxAttachResource { .. } => "CTX_ATTACH_RESOURCE",
        Command::CtxDetachResource { .. } => "CTX_DETACH_RESOURCE",
        Command::ResourceCreate3d(_) => "RESOURCE_CREATE_3D",
        Command::TransferToHost3d(_) => "TRANSFER_TO_HOST_3D",
        Command::TransferFromHost3d(_) => "TRANSFER_FROM_HOST_3D",
        Command::Submit3d { .. } => "SUBMIT_3D",
        Command::ResourceMapBlob { .. } => "RESOURCE_MAP_BLOB",
        Command::ResourceUnmapBlob { .. } => "RESOURCE_UNMAP_BLOB",
        Command::UpdateCursor(_) => "UPDATE_CURSOR",
        Command::MoveCursor(_) => "MOVE_CURSOR",
    }
}

fn error_name(code: u32) -> &'static str {
    match code {
        cmd::RESP_ERR_OUT_OF_MEMORY => "out of memory",
        cmd::RESP_ERR_INVALID_SCANOUT_ID => "invalid scanout",
        cmd::RESP_ERR_INVALID_RESOURCE_ID => "invalid resource",
        cmd::RESP_ERR_INVALID_CONTEXT_ID => "invalid context",
        cmd::RESP_ERR_INVALID_PARAMETER => "invalid parameter",
        _ => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::protocol::{MAX_ENTRIES, capset};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Fake guest RAM: one region at `base`, plus a log of what was mapped.
    struct Ram {
        base: u64,
        bytes: Rc<RefCell<Vec<u8>>>,
        maps: RefCell<Vec<(u64, usize)>>,
    }

    struct Piece {
        ptr: *const u8,
        len: usize,
        _keep: Rc<RefCell<Vec<u8>>>,
    }
    impl Mapping for Piece {
        fn as_ptr(&self) -> *const u8 {
            self.ptr
        }
        fn len(&self) -> usize {
            self.len
        }
    }

    impl Ram {
        fn new(base: u64, size: usize) -> Self {
            Self { base, bytes: Rc::new(RefCell::new(vec![0; size])), maps: RefCell::new(Vec::new()) }
        }
        fn write(&self, addr: u64, data: &[u8]) {
            let at = (addr - self.base) as usize;
            self.bytes.borrow_mut()[at..at + data.len()].copy_from_slice(data);
        }
    }

    impl GuestMemory for Ram {
        fn map(&self, addr: u64, len: usize) -> Option<Box<dyn Mapping>> {
            let at = addr.checked_sub(self.base)? as usize;
            if at.checked_add(len)? > self.bytes.borrow().len() {
                return None;
            }
            self.maps.borrow_mut().push((addr, len));
            let ptr = unsafe { self.bytes.borrow().as_ptr().add(at) };
            Some(Box::new(Piece { ptr, len, _keep: self.bytes.clone() }))
        }
    }

    const BASE: u64 = 0x7000_0000;

    fn frames() -> &'static Frames {
        Box::leak(Box::new(Frames::new()))
    }

    fn server() -> Server {
        Server::new(vec![(1920, 1080)], vec![], 64 << 20, frames())
    }

    fn send(s: &mut Server, mem: &dyn GuestMemory, cmd: Command) -> Response {
        let req = Request { hdr: CtrlHdr { flags: crate::gpu::protocol::FLAG_FENCE, fence_id: 77, ..Default::default() }, cmd };
        let (hdr, resp) = Response::parse(&s.handle(&req.serialize(), mem)).unwrap();
        assert_eq!(hdr.fence_id, 77, "fence echoed");
        resp
    }

    fn ok(s: &mut Server, mem: &dyn GuestMemory, cmd: Command) {
        let c = format!("{cmd:?}");
        assert_eq!(send(s, mem, cmd), Response::OkNoData, "{c}");
    }

    fn err(s: &mut Server, mem: &dyn GuestMemory, cmd: Command) -> u32 {
        match send(s, mem, cmd) {
            Response::Err(code) => code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// A resource with its backing at BASE as `pages` 4 KiB entries, in
    /// reverse page order, so scatter-gather actually scatters.
    fn make(s: &mut Server, ram: &Ram, id: u32, format: Format, w: u32, h: u32) -> Vec<MemEntry> {
        ok(s, ram, Command::ResourceCreate2d { resource_id: id, format: format as u32, width: w, height: h });
        let bytes = (w * h * 4) as u64;
        let pages = bytes.div_ceil(4096);
        let entries: Vec<MemEntry> = (0..pages)
            .map(|p| MemEntry { addr: BASE + (pages - 1 - p) * 4096, length: (bytes - p * 4096).min(4096) as u32 })
            .collect();
        ok(s, ram, Command::ResourceAttachBacking { resource_id: id, entries: entries.clone() });
        entries
    }

    /// Write pixel (x, y) of a resource backed by `make`'s layout.
    fn put(ram: &Ram, w: u32, h: u32, x: u32, y: u32, px: [u8; 4]) {
        let pages = ((w * h * 4) as u64).div_ceil(4096);
        let off = ((y * w + x) * 4) as u64;
        let (page, within) = (off / 4096, off % 4096);
        ram.write(BASE + (pages - 1 - page) * 4096 + within, &px);
    }

    #[test]
    fn display_info_edid_and_capsets() {
        let ram = Ram::new(BASE, 4096);
        let mut s = Server::new(
            vec![(1280, 800), (640, 480)],
            vec![Capset { id: capset::VIRGL2, max_version: 2, data: vec![9; 10] }],
            1 << 20,
            frames(),
        );
        let Response::OkDisplayInfo(modes) = send(&mut s, &ram, Command::GetDisplayInfo) else { panic!() };
        assert_eq!(modes.len(), 16);
        assert_eq!((modes[0].rect, modes[0].enabled), (Rect::new(0, 0, 1280, 800), true));
        assert_eq!((modes[1].rect.width, modes[2].enabled), (640, false));
        let Response::OkEdid(e) = send(&mut s, &ram, Command::GetEdid { scanout: 1 }) else { panic!() };
        assert_eq!(e, edid(640, 480).to_vec());
        assert_eq!(err(&mut s, &ram, Command::GetEdid { scanout: 2 }), cmd::RESP_ERR_INVALID_PARAMETER);
        assert_eq!(
            send(&mut s, &ram, Command::GetCapsetInfo { capset_index: 0 }),
            Response::OkCapsetInfo { capset_id: capset::VIRGL2, max_version: 2, max_size: 10 }
        );
        assert_eq!(err(&mut s, &ram, Command::GetCapsetInfo { capset_index: 1 }), cmd::RESP_ERR_INVALID_PARAMETER);
        assert_eq!(send(&mut s, &ram, Command::GetCapset { capset_id: capset::VIRGL2, capset_version: 1 }), Response::OkCapset(vec![9; 10]));
        assert_eq!(err(&mut s, &ram, Command::GetCapset { capset_id: capset::VIRGL2, capset_version: 3 }), cmd::RESP_ERR_INVALID_PARAMETER);
        assert_eq!(err(&mut s, &ram, Command::GetCapset { capset_id: capset::VENUS, capset_version: 0 }), cmd::RESP_ERR_INVALID_PARAMETER);
    }

    #[test]
    fn resource_table() {
        let ram = Ram::new(BASE, 1 << 20);
        let mut s = Server::new(vec![(64, 64)], vec![], 64 * 64 * 4 * 2, frames());
        let create = |id, format, w, h| Command::ResourceCreate2d { resource_id: id, format, width: w, height: h };
        ok(&mut s, &ram, create(1, 2, 64, 64));
        assert_eq!(err(&mut s, &ram, create(1, 2, 8, 8)), cmd::RESP_ERR_INVALID_RESOURCE_ID, "duplicate");
        assert_eq!(err(&mut s, &ram, create(0, 2, 8, 8)), cmd::RESP_ERR_INVALID_RESOURCE_ID, "id 0");
        assert_eq!(err(&mut s, &ram, create(2, 5, 8, 8)), cmd::RESP_ERR_INVALID_PARAMETER, "format");
        assert_eq!(err(&mut s, &ram, create(2, 2, 0, 8)), cmd::RESP_ERR_INVALID_PARAMETER, "empty");
        assert_eq!(err(&mut s, &ram, create(2, 2, MAX_DIMENSION + 1, 1)), cmd::RESP_ERR_INVALID_PARAMETER, "too wide");
        ok(&mut s, &ram, create(2, 1, 64, 64));
        assert_eq!(err(&mut s, &ram, create(3, 1, 1, 1)), cmd::RESP_ERR_OUT_OF_MEMORY, "host memory cap");
        ok(&mut s, &ram, Command::ResourceUnref { resource_id: 2 });
        ok(&mut s, &ram, create(3, 1, 64, 64));
        assert_eq!(err(&mut s, &ram, Command::ResourceUnref { resource_id: 2 }), cmd::RESP_ERR_INVALID_RESOURCE_ID);

        // Backing: once, with valid entries, in guest RAM.
        let entry = MemEntry { addr: BASE, length: 4096 };
        let attach = |id, entries: Vec<MemEntry>| Command::ResourceAttachBacking { resource_id: id, entries };
        assert_eq!(err(&mut s, &ram, attach(9, vec![entry])), cmd::RESP_ERR_INVALID_RESOURCE_ID);
        assert_eq!(err(&mut s, &ram, attach(1, vec![])), cmd::RESP_ERR_INVALID_PARAMETER);
        assert_eq!(err(&mut s, &ram, attach(1, vec![MemEntry { addr: BASE, length: 0 }])), cmd::RESP_ERR_INVALID_PARAMETER);
        assert_eq!(err(&mut s, &ram, attach(1, vec![MemEntry { addr: 0x10, length: 4096 }])), cmd::RESP_ERR_UNSPEC, "not RAM");
        assert_eq!(err(&mut s, &ram, attach(1, vec![MemEntry { addr: BASE + (1 << 20) - 10, length: 4096 }])), cmd::RESP_ERR_UNSPEC, "runs off RAM");
        ok(&mut s, &ram, attach(1, vec![entry]));
        assert_eq!(err(&mut s, &ram, attach(1, vec![entry])), cmd::RESP_ERR_UNSPEC, "already attached");
        ok(&mut s, &ram, Command::ResourceDetachBacking { resource_id: 1 });
        assert_eq!(err(&mut s, &ram, Command::ResourceDetachBacking { resource_id: 1 }), cmd::RESP_ERR_UNSPEC);
        assert_eq!(err(&mut s, &ram, Command::ResourceDetachBacking { resource_id: 7 }), cmd::RESP_ERR_INVALID_RESOURCE_ID);

        // UUIDs are stable per resource.
        let Response::OkResourceUuid(a) = send(&mut s, &ram, Command::ResourceAssignUuid { resource_id: 1 }) else { panic!() };
        let Response::OkResourceUuid(b) = send(&mut s, &ram, Command::ResourceAssignUuid { resource_id: 1 }) else { panic!() };
        assert_eq!(a, b);
        assert_eq!(a[6] >> 4, 4);
        assert_eq!(err(&mut s, &ram, Command::ResourceAssignUuid { resource_id: 8 }), cmd::RESP_ERR_INVALID_RESOURCE_ID);
    }

    #[test]
    fn contiguous_entries_are_mapped_once() {
        let ram = Ram::new(BASE, 1 << 20);
        let mut s = server();
        ok(&mut s, &ram, Command::ResourceCreate2d { resource_id: 1, format: 2, width: 64, height: 64 });
        let entries = (0..4).map(|p| MemEntry { addr: BASE + p * 4096, length: 4096 }).collect();
        ok(&mut s, &ram, Command::ResourceAttachBacking { resource_id: 1, entries });
        assert_eq!(*ram.maps.borrow(), vec![(BASE, 4 * 4096)]);
    }

    /// Guest RAM in two regions that touch: a mapping can't cross from one
    /// to the other, so a contiguous run over the seam is mapped per entry.
    #[test]
    fn runs_across_ram_regions_fall_back_to_entries() {
        struct Split<'a>(&'a Ram, u64);
        impl GuestMemory for Split<'_> {
            fn map(&self, addr: u64, len: usize) -> Option<Box<dyn Mapping>> {
                if addr < self.1 && addr + len as u64 > self.1 {
                    return None;
                }
                self.0.map(addr, len)
            }
        }
        let ram = Ram::new(BASE, 1 << 20);
        let split = Split(&ram, BASE + 8192);
        let mut s = server();
        ok(&mut s, &split, Command::ResourceCreate2d { resource_id: 1, format: 1, width: 16, height: 64 });
        let entries = (0..4).map(|p| MemEntry { addr: BASE + p * 1024, length: 1024 }).chain((8..12).map(|p| MemEntry { addr: BASE + p * 1024, length: 1024 })).collect();
        ok(&mut s, &split, Command::ResourceAttachBacking { resource_id: 1, entries });
        assert_eq!(*ram.maps.borrow(), vec![(BASE, 4096), (BASE + 8192, 4096)]);
        let across: Vec<MemEntry> = (6..10).map(|p| MemEntry { addr: BASE + p * 1024, length: 1024 }).collect();
        ok(&mut s, &split, Command::ResourceCreate2d { resource_id: 2, format: 1, width: 16, height: 64 });
        ram.maps.borrow_mut().clear();
        ok(&mut s, &split, Command::ResourceAttachBacking { resource_id: 2, entries: across });
        assert_eq!(ram.maps.borrow().len(), 4, "one mapping per entry");
        ram.write(BASE + 8192 - 4, &[1, 2, 3, 4, 5, 6, 7, 8]);
        // Bytes 2044..2052 of the backing straddle the seam.
        ok(&mut s, &split, Command::TransferToHost2d { rect: Rect::new(0, 0, 16, 64), offset: 0, resource_id: 2 });
        assert_eq!(&s.resources[&2].pixels[2044..2052], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn transfer_copies_rows_from_scattered_backing() {
        let ram = Ram::new(BASE, 1 << 20);
        let mut s = server();
        let (w, h) = (100, 50); // 20000 bytes: 5 pages, rows straddle pages
        make(&mut s, &ram, 1, Format::B8G8R8X8, w, h);
        for y in 0..h {
            for x in 0..w {
                put(&ram, w, h, x, y, [x as u8, y as u8, 0x55, 0]);
            }
        }
        // A rectangle in the middle, at the offset Linux computes.
        let rect = Rect::new(10, 5, 30, 20);
        let offset = (rect.y * w * 4 + rect.x * 4) as u64;
        ok(&mut s, &ram, Command::TransferToHost2d { rect, offset, resource_id: 1 });
        let res = &s.resources[&1];
        for y in 0..h {
            for x in 0..w {
                let px = &res.pixels[((y * w + x) * 4) as usize..][..4];
                let inside = (10..40).contains(&x) && (5..25).contains(&y);
                assert_eq!(px, if inside { [x as u8, y as u8, 0x55, 0] } else { [0; 4] }, "({x},{y})");
            }
        }
        // The whole resource.
        ok(&mut s, &ram, Command::TransferToHost2d { rect: Rect::new(0, 0, w, h), offset: 0, resource_id: 1 });
        assert_eq!(&s.resources[&1].pixels[((49 * w + 99) * 4) as usize..][..4], &[99, 49, 0x55, 0]);
        // An empty rectangle is fine and does nothing.
        ok(&mut s, &ram, Command::TransferToHost2d { rect: Rect::new(0, 0, 0, 0), offset: 0, resource_id: 1 });
    }

    #[test]
    fn transfers_reject_out_of_range_input() {
        let ram = Ram::new(BASE, 1 << 20);
        let mut s = server();
        let (w, h) = (64, 64);
        make(&mut s, &ram, 1, Format::B8G8R8A8, w, h);
        ok(&mut s, &ram, Command::ResourceCreate2d { resource_id: 2, format: 1, width: 8, height: 8 });
        let t = |rect, offset, id| Command::TransferToHost2d { rect, offset, resource_id: id };
        let before = s.resources[&1].pixels.clone();
        ram.write(BASE, &[0xff; 4096]);
        for (rect, offset) in [
            (Rect::new(1, 0, 64, 1), 0),                // past the right edge
            (Rect::new(0, 60, 1, 5), 0),                // past the bottom
            (Rect::new(u32::MAX, 0, 2, 1), 0),          // wrapping x
            (Rect::new(0, 0, 64, u32::MAX), 0),         // wrapping height
            (Rect::new(0, 63, 64, 1), w as u64 * 4 * 63 + 1), // reads one byte past the backing
            (Rect::new(0, 0, 1, 1), u64::MAX - 2),      // wrapping offset
        ] {
            assert_eq!(err(&mut s, &ram, t(rect, offset, 1)), cmd::RESP_ERR_INVALID_PARAMETER, "{rect:?} at {offset}");
        }
        assert_eq!(s.resources[&1].pixels, before, "failed transfers change nothing");
        assert_eq!(err(&mut s, &ram, t(Rect::new(0, 0, 1, 1), 0, 2)), cmd::RESP_ERR_UNSPEC, "no backing");
        assert_eq!(err(&mut s, &ram, t(Rect::new(0, 0, 1, 1), 0, 3)), cmd::RESP_ERR_INVALID_RESOURCE_ID);
        // A backing shorter than the resource: rows past it are refused.
        ok(&mut s, &ram, Command::ResourceAttachBacking { resource_id: 2, entries: vec![MemEntry { addr: BASE, length: 100 }] });
        ok(&mut s, &ram, t(Rect::new(0, 0, 8, 3), 0, 2));
        assert_eq!(err(&mut s, &ram, t(Rect::new(0, 0, 8, 4), 0, 2)), cmd::RESP_ERR_INVALID_PARAMETER);
    }

    #[test]
    fn hostile_requests_get_errors() {
        let ram = Ram::new(BASE, 4096);
        let mut s = server();
        // Too short for a header: still a well-formed error reply.
        let (_, r) = Response::parse(&s.handle(&[1, 2, 3], &ram)).unwrap();
        assert_eq!(r, Response::Err(cmd::RESP_ERR_UNSPEC));
        // Unknown command.
        let mut b = vec![0u8; 24];
        b[..4].copy_from_slice(&0x0777u32.to_le_bytes());
        assert_eq!(Response::parse(&s.handle(&b, &ram)).unwrap().1, Response::Err(cmd::RESP_ERR_UNSPEC));
        // Too many entries.
        let mut a = Request { hdr: CtrlHdr::default(), cmd: Command::ResourceAttachBacking { resource_id: 1, entries: vec![] } }.serialize();
        a[28..32].copy_from_slice(&(MAX_ENTRIES + 1).to_le_bytes());
        assert_eq!(Response::parse(&s.handle(&a, &ram)).unwrap().1, Response::Err(cmd::RESP_ERR_OUT_OF_MEMORY));
        // 3D and blobs aren't served.
        assert_eq!(err(&mut s, &ram, Command::CtxCreate { context_init: 0, debug_name: vec![] }), cmd::RESP_ERR_UNSPEC);
        assert_eq!(err(&mut s, &ram, Command::Submit3d { data: vec![0; 16] }), cmd::RESP_ERR_UNSPEC);
        assert_eq!(
            err(&mut s, &ram, Command::ResourceCreateBlob { resource_id: 5, blob_mem: 1, blob_flags: 0, blob_id: 0, size: 4096, entries: vec![] }),
            cmd::RESP_ERR_UNSPEC
        );
    }

    #[test]
    fn scanout_and_flush_publish_frames() {
        let ram = Ram::new(BASE, 1 << 20);
        let f = frames();
        let mut s = Server::new(vec![(64, 32)], vec![], 64 << 20, f);
        let (w, h) = (64, 32);
        make(&mut s, &ram, 1, Format::X8R8G8B8, w, h);
        // X8R8G8B8 in memory: X, R, G, B.
        put(&ram, w, h, 3, 2, [0, 0x11, 0x22, 0x33]);
        put(&ram, w, h, 60, 30, [0, 0x44, 0x55, 0x66]);
        ok(&mut s, &ram, Command::TransferToHost2d { rect: Rect::new(0, 0, w, h), offset: 0, resource_id: 1 });
        assert_eq!(err(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, w, h), scanout_id: 1, resource_id: 1 }), cmd::RESP_ERR_INVALID_SCANOUT_ID);
        assert_eq!(err(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, w, h), scanout_id: 0, resource_id: 9 }), cmd::RESP_ERR_INVALID_RESOURCE_ID);
        assert_eq!(err(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, w + 1, h), scanout_id: 0, resource_id: 1 }), cmd::RESP_ERR_INVALID_PARAMETER);
        ok(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, w, h), scanout_id: 0, resource_id: 1 });
        let frame = f.latest(0).unwrap();
        assert_eq!((frame.width, frame.height), (64, 32));
        let at = |fr: &frames::Frame, x: u32, y: u32| fr.pixels[((y * fr.width + x) * 4) as usize..][..4].to_vec();
        assert_eq!(at(&frame, 3, 2), [0x33, 0x22, 0x11, 0xff], "BGRA, opaque");

        // A flush publishes only after a transfer, and only its rectangle.
        put(&ram, w, h, 3, 2, [0, 1, 2, 3]);
        put(&ram, w, h, 60, 30, [0, 4, 5, 6]);
        ok(&mut s, &ram, Command::TransferToHost2d { rect: Rect::new(0, 0, w, h), offset: 0, resource_id: 1 });
        ok(&mut s, &ram, Command::ResourceFlush { rect: Rect::new(0, 0, 10, 10), resource_id: 1 });
        let next = f.latest(0).unwrap();
        assert!(next.sequence > frame.sequence);
        assert_eq!(at(&next, 3, 2), [3, 2, 1, 0xff]);
        assert_eq!(at(&next, 60, 30), [0x66, 0x55, 0x44, 0xff], "outside the flushed rect");
        assert_eq!(at(&frame, 3, 2), [0x33, 0x22, 0x11, 0xff], "a held frame is unchanged");
        assert_eq!(err(&mut s, &ram, Command::ResourceFlush { rect: Rect::new(0, 0, 65, 1), resource_id: 1 }), cmd::RESP_ERR_INVALID_PARAMETER);

        // Showing part of a resource: frame coordinates start at the rect.
        ok(&mut s, &ram, Command::SetScanout { rect: Rect::new(50, 20, 14, 12), scanout_id: 0, resource_id: 1 });
        let part = f.latest(0).unwrap();
        assert_eq!((part.width, part.height), (14, 12));
        assert_eq!(at(&part, 10, 10), [6, 5, 4, 0xff]);

        // Unref of the shown resource turns the scanout off.
        ok(&mut s, &ram, Command::ResourceUnref { resource_id: 1 });
        assert!(f.latest(0).is_none());
        make(&mut s, &ram, 2, Format::B8G8R8A8, 8, 8);
        ok(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, 8, 8), scanout_id: 0, resource_id: 2 });
        assert!(f.latest(0).is_some());
        ok(&mut s, &ram, Command::SetScanout { rect: Rect::default(), scanout_id: 0, resource_id: 0 });
        assert!(f.latest(0).is_none());
    }

    #[test]
    fn every_format_converts_to_bgra() {
        let px = [0x10, 0x20, 0x30, 0x40];
        let mut out = [0u8; 4];
        let mut check = |f: Format, want: [u8; 4]| {
            convert(&px, &mut out, f);
            assert_eq!(out, want, "{f:?}");
        };
        check(Format::B8G8R8A8, [0x10, 0x20, 0x30, 0x40]);
        check(Format::B8G8R8X8, [0x10, 0x20, 0x30, 0xff]);
        check(Format::A8R8G8B8, [0x40, 0x30, 0x20, 0x10]);
        check(Format::X8R8G8B8, [0x40, 0x30, 0x20, 0xff]);
        check(Format::R8G8B8A8, [0x30, 0x20, 0x10, 0x40]);
        check(Format::R8G8B8X8, [0x30, 0x20, 0x10, 0xff]);
        check(Format::A8B8G8R8, [0x20, 0x30, 0x40, 0x10]);
        check(Format::X8B8G8R8, [0x20, 0x30, 0x40, 0xff]);
    }

    #[test]
    fn cursor_commands() {
        let ram = Ram::new(BASE, 1 << 20);
        let f = frames();
        let mut s = Server::new(vec![(640, 480)], vec![], 64 << 20, f);
        make(&mut s, &ram, 5, Format::B8G8R8A8, 64, 64);
        put(&ram, 64, 64, 0, 0, [1, 2, 3, 4]);
        ok(&mut s, &ram, Command::TransferToHost2d { rect: Rect::new(0, 0, 64, 64), offset: 0, resource_id: 5 });
        let c = CursorCmd { scanout_id: 0, x: 100, y: 200, resource_id: 5, hot_x: 3, hot_y: 99 };
        ok(&mut s, &ram, Command::UpdateCursor(c));
        let cur = f.cursor();
        let img = cur.image.unwrap();
        assert_eq!((cur.x, cur.y, img.width, img.hot_x, img.hot_y), (100, 200, 64, 3, 63));
        assert_eq!(&img.pixels[..4], &[1, 2, 3, 4]);
        ok(&mut s, &ram, Command::MoveCursor(CursorCmd { x: 5, y: 6, ..c }));
        assert_eq!((f.cursor().x, f.cursor().y), (5, 6));
        assert_eq!(err(&mut s, &ram, Command::MoveCursor(CursorCmd { scanout_id: 3, ..c })), cmd::RESP_ERR_INVALID_SCANOUT_ID);
        assert_eq!(err(&mut s, &ram, Command::UpdateCursor(CursorCmd { resource_id: 42, ..c })), cmd::RESP_ERR_INVALID_RESOURCE_ID);
        ok(&mut s, &ram, Command::UpdateCursor(CursorCmd { resource_id: 0, ..c }));
        assert!(f.cursor().image.is_none());
        ok(&mut s, &ram, Command::ResourceCreate2d { resource_id: 6, format: 1, width: 512, height: 512 });
        assert_eq!(err(&mut s, &ram, Command::UpdateCursor(CursorCmd { resource_id: 6, ..c })), cmd::RESP_ERR_INVALID_PARAMETER);
    }

    #[test]
    fn save_and_restore() {
        let ram = Ram::new(BASE, 1 << 20);
        let f = frames();
        let mut s = Server::new(vec![(32, 32)], vec![], 64 << 20, f);
        make(&mut s, &ram, 1, Format::B8G8R8X8, 32, 32);
        ok(&mut s, &ram, Command::ResourceCreate2d { resource_id: 2, format: 1, width: 8, height: 8 });
        put(&ram, 32, 32, 31, 31, [7, 8, 9, 0]);
        ok(&mut s, &ram, Command::TransferToHost2d { rect: Rect::new(0, 0, 32, 32), offset: 0, resource_id: 1 });
        ok(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, 32, 32), scanout_id: 0, resource_id: 1 });
        let Response::OkResourceUuid(uuid) = send(&mut s, &ram, Command::ResourceAssignUuid { resource_id: 1 }) else { panic!() };
        let state = s.save_state();

        // A fresh device (as after a restore) rebuilds it all from guest RAM.
        let f2 = frames();
        let mut t = Server::new(vec![(32, 32)], vec![], 64 << 20, f2);
        assert!(t.restore_state(&state, &ram));
        assert_eq!(t.resources.len(), 2);
        assert_eq!(t.resources[&1].uuid, Some(uuid));
        assert!(t.resources[&2].backing.is_none());
        let frame = f2.latest(0).unwrap();
        assert_eq!(&frame.pixels[((31 * 32 + 31) * 4)..][..4], &[7, 8, 9, 0xff]);
        assert_eq!(t.host_bytes, s.host_bytes);

        // Guest RAM not mapped yet: the backing is kept and mapped on first use.
        let empty = Ram::new(0, 0);
        let mut u = Server::new(vec![(32, 32)], vec![], 64 << 20, frames());
        assert!(u.restore_state(&state, &empty));
        assert!(u.resources[&1].pending.is_some());
        ok(&mut u, &ram, Command::TransferToHost2d { rect: Rect::new(0, 0, 32, 32), offset: 0, resource_id: 1 });
        assert!(u.resources[&1].backing.is_some());

        assert!(!t.restore_state(b"nonsense", &ram));
        t.reset();
        assert!(t.resources.is_empty() && f2.latest(0).is_none());
    }
}
