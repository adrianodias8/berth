//! The virtio-gpu command server: the resource table, every 2D command, and
//! the routing of 3D, context and blob commands to the renderer
//! (backend.rs), independent of Virtualization.framework so it's tested here
//! with fake guest memory and a fake renderer. device.rs feeds it requests
//! off the virtqueues; it answers with a `Reply` and publishes what the
//! guest shows to frames.rs.
//!
//! Everything a request carries is untrusted. Resource sizes are capped
//! (`MAX_DIMENSION`, and the host memory all resources may take), every
//! rectangle and offset is checked against the resource and its backing
//! before a byte is copied (in u64, so hostile values can't wrap), and a
//! failed command changes nothing.
//!
//! **3D.** With no renderer, 3D, context and blob commands answer
//! ERR_UNSPEC, and the device offers none of VIRGL, CONTEXT_INIT or
//! RESOURCE_BLOB (device.rs), so a conforming guest never sends them. With
//! one, contexts and 3D resources live in the renderer and are mirrored
//! here (sizes, backing, which context holds what), so every id is checked
//! before the renderer sees it. A 3D resource on a scanout is read back
//! from the renderer into a host copy on SET_SCANOUT and RESOURCE_FLUSH,
//! then published like a 2D one: any KMS client on card0 reaches the Mac
//! with no other path.
//!
//! **Fences.** With a renderer, every fenced request (2D ones too) gets a
//! renderer fence and is answered when it signals (`Reply::Fenced`): the
//! guest's driver takes an answered fence as signalling every earlier one,
//! so a 2D reply can't overtake 3D work fenced before it. virglrenderer's
//! global fences are 32-bit while the guest's are 64, so the global
//! timeline uses a host sequence of its own; the reply keeps the guest's.
//! Per-context fences (FLAG_INFO_RING_IDX, with CONTEXT_INIT) use the
//! guest's fence_id directly.
//!
//! **Blobs** (only with `gpu_blob`): MAP_BLOB puts the renderer's host
//! memory into the shared memory region, which device.rs does
//! asynchronously (`Reply::Deferred`).

use std::collections::{HashMap, HashSet};

use super::backend::{Backend3d, BlobArgs, Iov, SignalSink};
use super::fence::Signal;
use super::frames::{self, CursorImage, Frames};
use super::protocol::{
    Command, Create3d, CtrlHdr, CursorCmd, F_CONTEXT_INIT, FLAG_FENCE, FLAG_INFO_RING_IDX, Format, MemEntry, ParseError, Rect,
    Request, Response, Transfer3d, blob_flags, blob_mem, capset, cmd, edid,
};

/// Largest width or height of a 2D resource (Linux's virtio_gpu caps its
/// mode config at 16384 too).
pub const MAX_DIMENSION: u32 = 16384;
/// Most guest memory one resource's backing may span.
const MAX_BACKING_BYTES: u64 = 4 << 30;
/// Cursor images are 64x64; anything over this isn't a cursor.
const MAX_CURSOR: u32 = 256;
/// Errors logged before going quiet (a broken guest could send millions).
const LOGGED_ERRORS: u32 = 50;
/// Contexts a guest may hold (Linux makes one per DRM client).
const MAX_CONTEXTS: usize = 1024;
/// The renderer's memory all 3D resources together may take, in multiples
/// of the host limit (one resource may take at most the host limit).
const GPU_LIMIT_FACTOR: u64 = 4;
/// Blob mappings go into the shared memory region in host pages.
pub const MAP_ALIGN: u64 = 16 << 10;
/// `target` of a buffer in RESOURCE_CREATE_3D (gallium's PIPE_BUFFER).
const PIPE_BUFFER: u32 = 0;

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

    /// Checked attach: entries present, none empty, not too much memory.
    fn attach(entries: Vec<MemEntry>, mem: &dyn GuestMemory) -> Result<Self, u32> {
        let total: u64 = entries.iter().map(|e| e.length as u64).sum();
        if entries.is_empty() || entries.iter().any(|e| e.length == 0) || total > MAX_BACKING_BYTES {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        Self::map(entries, mem).ok_or(cmd::RESP_ERR_UNSPEC)
    }

    /// The pieces as the renderer takes them. It writes through them too
    /// (TRANSFER_FROM_HOST_3D): guest RAM is mapped read-write.
    fn iovs(&self) -> Vec<Iov> {
        self.pieces.iter().map(|(m, len)| (m.as_ptr() as *mut u8, *len)).collect()
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

/// Where a mapped blob sits in the shared memory region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Mapped {
    offset: u64,
    len: u64,
    /// UNMAP_BLOB was asked; device.rs is taking it out of the region.
    leaving: bool,
}

/// How a blob on a scanout is laid out (SET_SCANOUT_BLOB).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BlobLayout {
    stride: u32,
    offset: u32,
}

enum Kind {
    /// The host copy (`pixels`) is filled by TRANSFER_TO_HOST_2D.
    TwoD,
    /// In the renderer; `pixels` is a read-back copy, made when shown.
    ThreeD { gpu_bytes: u64 },
    Blob { mem: u32, flags: u32, size: u64, mapped: Option<Mapped>, layout: Option<BlobLayout> },
}

struct Resource {
    kind: Kind,
    /// The pixel layout; None for 3D resources (and blobs not yet shown)
    /// in a format the display doesn't show.
    format: Option<Format>,
    width: u32,
    height: u32,
    /// The host copy, in the guest's format, rows of `width * 4` bytes.
    /// For 3D and blob resources, the renderer's rows as a transfer reads
    /// them back, top-down whatever Y_0_TOP says (virglrenderer flips those
    /// on readback), empty until the resource is shown.
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
    fn in_renderer(&self) -> bool {
        !matches!(self.kind, Kind::TwoD)
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

/// A renderer context.
struct Context {
    resources: HashSet<u32>,
}

/// What device.rs does with a request's reply.
pub enum Reply {
    /// Write it back now.
    Now(Vec<u8>),
    /// Write it back once the renderer signals `on`.
    Fenced { on: Signal, bytes: Vec<u8> },
    /// Write it back once the shared memory region has done `op`.
    Deferred { op: Deferred, bytes: Vec<u8> },
}

#[cfg(test)]
impl Reply {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Reply::Now(b) | Reply::Fenced { bytes: b, .. } | Reply::Deferred { bytes: b, .. } => b,
        }
    }
}

/// Shared memory region work for device.rs, which reports back with
/// `Server::map_failed` or `Server::unmapped`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Deferred {
    /// Map `len` bytes of host memory at `ptr` at `offset` in the region.
    Map { resource_id: u32, offset: u64, ptr: *mut u8, len: u64 },
    /// Take `len` bytes at `offset` out of the region.
    Unmap { resource_id: u32, offset: u64, len: u64 },
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
    backend: Option<Box<dyn Backend3d>>,
    contexts: HashMap<u32, Context>,
    /// The renderer's memory 3D resources are estimated to take.
    gpu_bytes: u64,
    /// The shared memory region's size, when blob resources are offered.
    blob_region: Option<u64>,
    /// Feature bits the guest accepted (CONTEXT_INIT changes contexts and
    /// fences).
    negotiated: u32,
    /// The last host sequence handed to a global fence.
    fence_seq: u32,
    /// Set by MAP_BLOB and UNMAP_BLOB for `handle` to return.
    deferred: Option<Deferred>,
    /// Mapped blobs a reset removed, kept in the renderer until device.rs
    /// has them out of the region, with the guest memory the renderer
    /// still has as their iovecs.
    orphans: HashMap<u32, Option<Backing>>,
    /// Contexts CTX_DESTROY removed since device.rs last asked: their ring
    /// fences will never signal.
    destroyed: Vec<u32>,
}

type Answer = Result<Response, u32>;

impl Server {
    /// `modes`: the size of each scanout (1 to 16); `host_limit`: bytes all
    /// resources together may take on the Mac.
    pub fn new(modes: Vec<(u32, u32)>, capsets: Vec<Capset>, host_limit: u64, frames: &'static Frames) -> Self {
        assert!((1..=super::protocol::MAX_SCANOUTS).contains(&modes.len()));
        let scanouts = vec![None; modes.len()];
        Self {
            modes,
            capsets,
            resources: HashMap::new(),
            scanouts,
            cursor: None,
            host_bytes: 0,
            host_limit,
            frames,
            errors: 0,
            backend: None,
            contexts: HashMap::new(),
            gpu_bytes: 0,
            blob_region: None,
            negotiated: 0,
            fence_seq: 0,
            deferred: None,
            orphans: HashMap::new(),
            destroyed: Vec::new(),
        }
    }

    /// Serve 3D through `backend`, offering its capsets.
    pub fn with_backend(mut self, mut backend: Box<dyn Backend3d>) -> Self {
        self.capsets = backend.capsets();
        self.backend = Some(backend);
        self
    }

    /// Serve blob resources, mapped into a shared memory region of
    /// `region` bytes. Only with a renderer.
    pub fn with_blobs(mut self, region: u64) -> Self {
        if self.backend.is_some() {
            self.blob_region = Some(region);
        }
        self
    }

    pub fn has_backend(&self) -> bool {
        self.backend.is_some()
    }

    pub fn blobs(&self) -> bool {
        self.blob_region.is_some()
    }

    /// The features the guest accepted, once it has (DRIVER_OK).
    pub fn set_negotiated(&mut self, features: u32) {
        self.negotiated = features;
    }

    /// Contexts destroyed since the last call. The renderer drops their
    /// ring fences unsignalled, so device.rs answers the requests waiting
    /// on them itself.
    pub fn take_destroyed(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.destroyed)
    }

    pub fn set_signal_sink(&mut self, sink: SignalSink) {
        if let Some(b) = &mut self.backend {
            b.set_signal_sink(sink);
        }
    }

    pub fn num_scanouts(&self) -> u32 {
        self.modes.len() as u32
    }

    pub fn num_capsets(&self) -> u32 {
        self.capsets.len() as u32
    }

    /// Serve one request (the cursor queue's requests have no room for a
    /// reply, and device.rs drops it then).
    pub fn handle(&mut self, request: &[u8], mem: &dyn GuestMemory) -> Reply {
        let hdr = match CtrlHdr::parse(request) {
            Ok(hdr) => hdr,
            Err(err) => {
                self.log(format_args!("{err}"));
                return Reply::Now(Response::Err(cmd::RESP_ERR_UNSPEC).serialize(&CtrlHdr::default()));
            }
        };
        let answer = match Request::parse(request) {
            Ok(req) => {
                let name = command_name(&req.cmd);
                let answer = self.execute(&req.hdr, req.cmd, mem);
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
        let bytes = answer.unwrap_or_else(Response::Err).serialize(&hdr);
        if let Some(op) = self.deferred.take() {
            return Reply::Deferred { op, bytes };
        }
        if hdr.flags & FLAG_FENCE != 0 && self.backend.is_some() {
            return self.fence(&hdr, bytes);
        }
        Reply::Now(bytes)
    }

    /// A renderer fence for a fenced request, failed ones included (an
    /// error answered early would still signal earlier fences).
    fn fence(&mut self, hdr: &CtrlHdr, bytes: Vec<u8>) -> Reply {
        let on_ring = hdr.flags & FLAG_INFO_RING_IDX != 0 && self.negotiated & F_CONTEXT_INIT != 0;
        let backend = self.backend.as_mut().unwrap();
        let (on, made) = if on_ring {
            let (ctx, ring) = (hdr.ctx_id, hdr.ring_idx as u32);
            let made = if self.contexts.contains_key(&ctx) {
                backend.create_context_fence(ctx, ring, hdr.fence_id)
            } else {
                Err(libc::EINVAL)
            };
            (Signal::Ring { ctx, ring, fence: hdr.fence_id }, made)
        } else {
            // 0 is never used, so the first signal can't be mistaken for it.
            self.fence_seq = self.fence_seq.wrapping_add(1).max(1);
            (Signal::Global(self.fence_seq), backend.create_fence(self.fence_seq))
        };
        match made {
            Ok(()) => Reply::Fenced { on, bytes },
            Err(e) => {
                self.log(format_args!("fence {} (ctx {}): renderer error {e}", hdr.fence_id, hdr.ctx_id));
                Reply::Now(bytes)
            }
        }
    }

    fn log(&mut self, what: std::fmt::Arguments) {
        if self.errors < LOGGED_ERRORS {
            eprintln!("gpu: {what}");
        } else if self.errors == LOGGED_ERRORS {
            eprintln!("gpu: more errors; not logging them");
        }
        self.errors = self.errors.saturating_add(1);
    }

    fn execute(&mut self, hdr: &CtrlHdr, command: Command, mem: &dyn GuestMemory) -> Answer {
        use cmd::*;
        let three_d = matches!(
            command,
            Command::CtxCreate { .. }
                | Command::CtxDestroy
                | Command::CtxAttachResource { .. }
                | Command::CtxDetachResource { .. }
                | Command::ResourceCreate3d(_)
                | Command::TransferToHost3d(_)
                | Command::TransferFromHost3d(_)
                | Command::Submit3d { .. }
        );
        let blob = matches!(
            command,
            Command::ResourceCreateBlob { .. } | Command::SetScanoutBlob { .. } | Command::ResourceMapBlob { .. } | Command::ResourceUnmapBlob { .. }
        );
        // Not offered (see the module comment).
        if (three_d && self.backend.is_none()) || (blob && self.blob_region.is_none()) {
            return Err(RESP_ERR_UNSPEC);
        }
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
            Command::ResourceUnref { resource_id } => self.unref(resource_id).map(|()| Response::OkNoData),
            Command::ResourceAttachBacking { resource_id, entries } => {
                self.attach(resource_id, entries, mem).map(|()| Response::OkNoData)
            }
            Command::ResourceDetachBacking { resource_id } => self.detach(resource_id).map(|()| Response::OkNoData),
            Command::TransferToHost2d { rect, offset, resource_id } => {
                self.transfer(resource_id, rect, offset, mem).map(|()| Response::OkNoData)
            }
            Command::SetScanout { rect, scanout_id, resource_id } => {
                self.set_scanout(scanout_id, resource_id, rect).map(|()| Response::OkNoData)
            }
            Command::ResourceFlush { rect, resource_id } => self.flush(resource_id, rect).map(|()| Response::OkNoData),
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
            Command::CtxCreate { context_init, debug_name } => {
                self.ctx_create(hdr.ctx_id, context_init, &debug_name).map(|()| Response::OkNoData)
            }
            Command::CtxDestroy => {
                self.contexts.remove(&hdr.ctx_id).ok_or(RESP_ERR_INVALID_CONTEXT_ID)?;
                self.backend.as_mut().unwrap().ctx_destroy(hdr.ctx_id);
                self.destroyed.push(hdr.ctx_id);
                Ok(Response::OkNoData)
            }
            Command::CtxAttachResource { resource_id } => {
                self.ctx_resource(hdr.ctx_id, resource_id, true).map(|()| Response::OkNoData)
            }
            Command::CtxDetachResource { resource_id } => {
                self.ctx_resource(hdr.ctx_id, resource_id, false).map(|()| Response::OkNoData)
            }
            Command::ResourceCreate3d(c) => self.create_3d(c).map(|()| Response::OkNoData),
            Command::TransferToHost3d(t) => self.transfer_3d(hdr.ctx_id, t, true).map(|()| Response::OkNoData),
            Command::TransferFromHost3d(t) => self.transfer_3d(hdr.ctx_id, t, false).map(|()| Response::OkNoData),
            Command::Submit3d { data } => {
                if !self.contexts.contains_key(&hdr.ctx_id) {
                    return Err(RESP_ERR_INVALID_CONTEXT_ID);
                }
                // The stream is 32-bit words (parsing checked it's all there).
                if data.len() % 4 != 0 {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                self.backend.as_mut().unwrap().submit_3d(hdr.ctx_id, &data).map_err(|_| RESP_ERR_UNSPEC)?;
                Ok(Response::OkNoData)
            }
            Command::ResourceCreateBlob { resource_id, blob_mem, blob_flags, blob_id, size, entries } => {
                let args = BlobArgs { resource_id, ctx_id: hdr.ctx_id, blob_mem, blob_flags, blob_id, size };
                self.create_blob(args, entries, mem).map(|()| Response::OkNoData)
            }
            Command::SetScanoutBlob { rect, scanout_id, resource_id, width, height, format, strides, offsets } => {
                let layout = BlobLayout { stride: strides[0], offset: offsets[0] };
                self.set_scanout_blob(scanout_id, resource_id, rect, (width, height, format), layout).map(|()| Response::OkNoData)
            }
            Command::ResourceMapBlob { resource_id, offset } => self.map_blob(resource_id, offset),
            Command::ResourceUnmapBlob { resource_id } => {
                let res = self.resources.get_mut(&resource_id).ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                let Kind::Blob { mapped: Some(m), .. } = &mut res.kind else { return Err(RESP_ERR_INVALID_PARAMETER) };
                if m.leaving {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                m.leaving = true;
                self.deferred = Some(Deferred::Unmap { resource_id, offset: m.offset, len: m.len });
                Ok(Response::OkNoData)
            }
        }
    }

    fn new_id(&self, id: u32) -> Result<(), u32> {
        if id == 0 || self.resources.contains_key(&id) || self.orphans.contains_key(&id) {
            return Err(cmd::RESP_ERR_INVALID_RESOURCE_ID);
        }
        Ok(())
    }

    fn create(&mut self, id: u32, format: u32, width: u32, height: u32) -> Result<(), u32> {
        self.new_id(id)?;
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
        self.resources.insert(
            id,
            Resource { kind: Kind::TwoD, format: Some(format), width, height, pixels, backing: None, pending: None, uuid: None },
        );
        Ok(())
    }

    /// RESOURCE_UNREF of any kind. A mapped blob must be unmapped first:
    /// the region would otherwise keep host memory the renderer frees.
    fn unref(&mut self, id: u32) -> Result<(), u32> {
        let res = self.resources.get(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if matches!(res.kind, Kind::Blob { mapped: Some(_), .. }) {
            return Err(cmd::RESP_ERR_UNSPEC);
        }
        // Dropped at the end, after the renderer let go of its backing.
        let res = self.resources.remove(&id).unwrap();
        self.host_bytes -= res.pixels.len() as u64;
        if let Kind::ThreeD { gpu_bytes, .. } = res.kind {
            self.gpu_bytes -= gpu_bytes;
        }
        if res.in_renderer() {
            self.backend.as_mut().unwrap().resource_unref(id);
        }
        for ctx in self.contexts.values_mut() {
            ctx.resources.remove(&id);
        }
        for (i, s) in self.scanouts.iter_mut().enumerate() {
            if s.is_some_and(|b| b.resource_id == id) {
                *s = None;
                self.frames.remove(i as u32);
            }
        }
        Ok(())
    }

    fn attach(&mut self, id: u32, entries: Vec<MemEntry>, mem: &dyn GuestMemory) -> Result<(), u32> {
        let res = self.resources.get_mut(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if res.backing.is_some() || res.pending.is_some() {
            return Err(cmd::RESP_ERR_UNSPEC);
        }
        let backing = Backing::attach(entries, mem)?;
        match res.kind {
            Kind::TwoD => {}
            Kind::ThreeD { .. } => self.backend.as_mut().unwrap().attach_iov(id, &backing.iovs()).map_err(|_| cmd::RESP_ERR_UNSPEC)?,
            // A blob's memory is given when it's made.
            Kind::Blob { .. } => return Err(cmd::RESP_ERR_UNSPEC),
        }
        res.backing = Some(backing);
        Ok(())
    }

    fn detach(&mut self, id: u32) -> Result<(), u32> {
        let res = self.resources.get_mut(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        match res.kind {
            Kind::TwoD => {
                if res.backing.take().is_none() && res.pending.take().is_none() {
                    return Err(cmd::RESP_ERR_UNSPEC);
                }
            }
            Kind::ThreeD { .. } => {
                if res.backing.is_none() {
                    return Err(cmd::RESP_ERR_UNSPEC);
                }
                self.backend.as_mut().unwrap().detach_iov(id);
                res.backing = None;
            }
            Kind::Blob { .. } => return Err(cmd::RESP_ERR_UNSPEC),
        }
        Ok(())
    }

    /// TRANSFER_TO_HOST_2D: rows of `rect` from the backing into the host
    /// copy. Row `r` of the rectangle is at `offset + r * stride` in the
    /// backing, which has the resource's layout (this is what Linux and
    /// QEMU do).
    fn transfer(&mut self, id: u32, rect: Rect, offset: u64, mem: &dyn GuestMemory) -> Result<(), u32> {
        let res = self.resources.get_mut(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if !matches!(res.kind, Kind::TwoD) || !rect.fits(res.width, res.height) {
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
        if scanout as usize >= self.scanouts.len() {
            return Err(cmd::RESP_ERR_INVALID_SCANOUT_ID);
        }
        if id == 0 {
            self.scanouts[scanout as usize] = None;
            self.frames.remove(scanout);
            return Ok(());
        }
        let res = self.resources.get(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if rect.is_empty() || !rect.fits(res.width, res.height) || res.format.is_none() {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        self.refresh(id, rect)?;
        let binding = Binding { resource_id: id, rect };
        self.scanouts[scanout as usize] = Some(binding);
        publish(self.frames, scanout, binding, &self.resources[&id], rect);
        Ok(())
    }

    fn flush(&mut self, id: u32, rect: Rect) -> Result<(), u32> {
        let res = self.resources.get(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if !rect.fits(res.width, res.height) {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        let shown: Vec<_> = self
            .scanouts
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b.filter(|b| b.resource_id == id).and_then(|b| rect.intersect(&b.rect).map(|d| (i as u32, b, d))))
            .collect();
        for (i, b, dirty) in shown {
            self.refresh(id, dirty)?;
            publish(self.frames, i, b, &self.resources[&id], dirty);
        }
        Ok(())
    }

    /// Bring the host copy of `rect` (image coordinates) up to date: a
    /// read-back from the renderer for 3D resources, a copy from guest
    /// memory for guest blobs. 2D ones are kept current by transfers.
    fn refresh(&mut self, id: u32, rect: Rect) -> Result<(), u32> {
        let res = self.resources.get_mut(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if matches!(res.kind, Kind::TwoD) || rect.is_empty() {
            return Ok(());
        }
        let stride = res.stride() as usize;
        if res.pixels.is_empty() {
            // One row of slack: the renderer may check room for whole rows.
            let bytes = res.bytes() + res.stride();
            if self.host_bytes + bytes > self.host_limit {
                return Err(cmd::RESP_ERR_OUT_OF_MEMORY);
            }
            self.host_bytes += bytes;
            res.pixels = vec![0u8; bytes as usize];
        }
        match res.kind {
            Kind::Blob { mem: blob_mem::GUEST, layout: Some(l), .. } => {
                let backing = res.backing.as_ref().ok_or(cmd::RESP_ERR_UNSPEC)?;
                let n = rect.width as usize * 4;
                for y in rect.y..rect.y + rect.height {
                    let src = l.offset as u64 + y as u64 * l.stride as u64 + rect.x as u64 * 4;
                    let at = y as usize * stride + rect.x as usize * 4;
                    if !backing.read(src, &mut res.pixels[at..at + n]) {
                        return Err(cmd::RESP_ERR_INVALID_PARAMETER);
                    }
                }
                Ok(())
            }
            _ => {
                let at = rect.y as usize * stride + rect.x as usize * 4;
                let backend = self.backend.as_mut().ok_or(cmd::RESP_ERR_UNSPEC)?;
                backend.read_back(id, rect, &mut res.pixels[at..], stride).map_err(|_| cmd::RESP_ERR_UNSPEC)
            }
        }
    }

    fn update_cursor(&mut self, c: CursorCmd) -> Result<(), u32> {
        if c.scanout_id >= self.num_scanouts() {
            return Err(cmd::RESP_ERR_INVALID_SCANOUT_ID);
        }
        let image = if c.resource_id == 0 {
            None
        } else {
            let res = self.resources.get(&c.resource_id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
            if res.width > MAX_CURSOR || res.height > MAX_CURSOR || res.format.is_none() {
                return Err(cmd::RESP_ERR_INVALID_PARAMETER);
            }
            let (w, h) = (res.width, res.height);
            self.refresh(c.resource_id, Rect::new(0, 0, w, h))?;
            let res = &self.resources[&c.resource_id];
            let mut pixels = vec![0u8; res.bytes() as usize];
            copy_rows(res, Rect::new(0, 0, w, h), &mut pixels, res.stride() as usize);
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

    fn ctx_create(&mut self, ctx: u32, context_init: u32, name: &[u8]) -> Result<(), u32> {
        if ctx == 0 || self.contexts.contains_key(&ctx) {
            return Err(cmd::RESP_ERR_INVALID_CONTEXT_ID);
        }
        if self.contexts.len() >= MAX_CONTEXTS {
            return Err(cmd::RESP_ERR_OUT_OF_MEMORY);
        }
        // Without CONTEXT_INIT (or with its capset left 0) it's virgl's.
        let wanted = context_init & 0xff;
        let capset_id = if self.negotiated & F_CONTEXT_INIT != 0 && wanted != 0 { wanted } else { capset::VIRGL2 };
        if !self.capsets.iter().any(|c| c.id == capset_id) {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        self.backend.as_mut().unwrap().ctx_create(ctx, capset_id, name).map_err(|_| cmd::RESP_ERR_UNSPEC)?;
        self.contexts.insert(ctx, Context { resources: HashSet::new() });
        Ok(())
    }

    /// CTX_ATTACH_RESOURCE / CTX_DETACH_RESOURCE. Linux attaches every
    /// buffer a client opens, 2D ones too; those the renderer doesn't know
    /// are only recorded.
    fn ctx_resource(&mut self, ctx: u32, id: u32, attach: bool) -> Result<(), u32> {
        let context = self.contexts.get_mut(&ctx).ok_or(cmd::RESP_ERR_INVALID_CONTEXT_ID)?;
        let res = self.resources.get(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        let changed = if attach { context.resources.insert(id) } else { context.resources.remove(&id) };
        if changed && res.in_renderer() {
            let backend = self.backend.as_mut().unwrap();
            if attach { backend.ctx_attach_resource(ctx, id) } else { backend.ctx_detach_resource(ctx, id) }
        }
        Ok(())
    }

    fn create_3d(&mut self, c: Create3d) -> Result<(), u32> {
        self.new_id(c.resource_id)?;
        let bad = if c.target == PIPE_BUFFER {
            c.width == 0 || c.height != 1 || c.width as u64 > self.host_limit
        } else {
            c.width == 0 || c.height == 0 || c.width > MAX_DIMENSION || c.height > MAX_DIMENSION || c.depth > MAX_DIMENSION
        };
        if bad || c.array_size > 2048 || c.last_level > 15 || c.nr_samples > 16 {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        // An estimate (4 bytes a texel, mip levels aside): it only has to
        // stop a guest from exhausting the Mac's memory.
        let gpu_bytes = if c.target == PIPE_BUFFER {
            c.width as u64
        } else {
            [c.width, c.height, c.depth.max(1), c.array_size.max(1), c.nr_samples.max(1), 4].iter().fold(1u64, |a, &n| a.saturating_mul(n as u64))
        };
        if gpu_bytes > self.host_limit || self.gpu_bytes + gpu_bytes > self.host_limit * GPU_LIMIT_FACTOR {
            return Err(cmd::RESP_ERR_OUT_OF_MEMORY);
        }
        self.backend.as_mut().unwrap().resource_create_3d(&c).map_err(|_| cmd::RESP_ERR_UNSPEC)?;
        self.gpu_bytes += gpu_bytes;
        // Only 2D textures are ever shown.
        let format = (c.target != PIPE_BUFFER).then(|| Format::from_u32(c.format)).flatten();
        let (width, height) = (c.width, c.height);
        let kind = Kind::ThreeD { gpu_bytes };
        self.resources.insert(c.resource_id, Resource { kind, format, width, height, pixels: Vec::new(), backing: None, pending: None, uuid: None });
        Ok(())
    }

    /// TRANSFER_TO_HOST_3D / TRANSFER_FROM_HOST_3D. The renderer checks the
    /// box against the resource and its backing.
    fn transfer_3d(&mut self, ctx: u32, t: Transfer3d, to_host: bool) -> Result<(), u32> {
        if ctx != 0 && !self.contexts.contains_key(&ctx) {
            return Err(cmd::RESP_ERR_INVALID_CONTEXT_ID);
        }
        let res = self.resources.get(&t.resource_id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        if !res.in_renderer() {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        let backend = self.backend.as_mut().unwrap();
        if to_host { backend.transfer_to_host_3d(ctx, &t) } else { backend.transfer_from_host_3d(ctx, &t) }.map_err(|_| cmd::RESP_ERR_UNSPEC)
    }

    fn create_blob(&mut self, args: BlobArgs, entries: Vec<MemEntry>, mem: &dyn GuestMemory) -> Result<(), u32> {
        self.new_id(args.resource_id)?;
        if args.size == 0 || args.size > MAX_BACKING_BYTES {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        let backing = match args.blob_mem {
            blob_mem::GUEST | blob_mem::HOST3D_GUEST => {
                let backing = Backing::attach(entries, mem)?;
                if backing.total < args.size {
                    return Err(cmd::RESP_ERR_INVALID_PARAMETER);
                }
                Some(backing)
            }
            blob_mem::HOST3D if entries.is_empty() => None,
            _ => return Err(cmd::RESP_ERR_INVALID_PARAMETER),
        };
        // Host blobs come out of a context's resources.
        if args.blob_mem != blob_mem::GUEST && !self.contexts.contains_key(&args.ctx_id) {
            return Err(cmd::RESP_ERR_INVALID_CONTEXT_ID);
        }
        let iovs = backing.as_ref().map(Backing::iovs).unwrap_or_default();
        self.backend.as_mut().unwrap().resource_create_blob(&args, &iovs).map_err(|_| cmd::RESP_ERR_UNSPEC)?;
        let kind = Kind::Blob { mem: args.blob_mem, flags: args.blob_flags, size: args.size, mapped: None, layout: None };
        self.resources.insert(args.resource_id, Resource { kind, format: None, width: 0, height: 0, pixels: Vec::new(), backing, pending: None, uuid: None });
        Ok(())
    }

    /// SET_SCANOUT_BLOB: the blob gets the size and format it's shown at,
    /// then is shown like any resource (host blobs are read back like 3D
    /// ones, guest blobs copied from their memory).
    fn set_scanout_blob(&mut self, scanout: u32, id: u32, rect: Rect, (width, height, format): (u32, u32, u32), layout: BlobLayout) -> Result<(), u32> {
        if scanout as usize >= self.scanouts.len() {
            return Err(cmd::RESP_ERR_INVALID_SCANOUT_ID);
        }
        if id == 0 {
            return self.set_scanout(scanout, 0, rect);
        }
        let res = self.resources.get_mut(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        let Kind::Blob { mem, size, layout: shown, .. } = &mut res.kind else { return Err(cmd::RESP_ERR_INVALID_RESOURCE_ID) };
        let format = Format::from_u32(format).ok_or(cmd::RESP_ERR_INVALID_PARAMETER)?;
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION || (layout.stride as u64) < width as u64 * 4 {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        let end = layout.offset as u64 + (height as u64 - 1) * layout.stride as u64 + width as u64 * 4;
        if *mem == blob_mem::GUEST && end > *size {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        *shown = Some(layout);
        if (res.width, res.height) != (width, height) {
            self.host_bytes -= res.pixels.len() as u64;
            res.pixels = Vec::new();
            (res.width, res.height) = (width, height);
        }
        res.format = Some(format);
        self.set_scanout(scanout, id, rect)
    }

    fn map_blob(&mut self, id: u32, offset: u64) -> Answer {
        let region = self.blob_region.unwrap();
        let res = self.resources.get_mut(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        let Kind::Blob { mem, flags, mapped, .. } = &mut res.kind else { return Err(cmd::RESP_ERR_INVALID_PARAMETER) };
        if *mem == blob_mem::GUEST || *flags & blob_flags::USE_MAPPABLE == 0 || mapped.is_some() {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        if !offset.is_multiple_of(MAP_ALIGN) || offset >= region {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        let backend = self.backend.as_mut().unwrap();
        let (ptr, len) = backend.resource_map(id).map_err(|_| cmd::RESP_ERR_UNSPEC)?;
        // The region maps whole host pages; anything less would show the
        // guest whatever shares the page.
        let fail = if !(ptr as u64).is_multiple_of(MAP_ALIGN) || len == 0 || !len.is_multiple_of(MAP_ALIGN) {
            Some(cmd::RESP_ERR_UNSPEC)
        } else if offset.checked_add(len).is_none_or(|end| end > region) {
            Some(cmd::RESP_ERR_INVALID_PARAMETER)
        } else {
            None
        };
        let overlaps = |o: u64, l: u64| o < offset + len && offset < o + l;
        let taken = self.resources.values().any(|r| matches!(r.kind, Kind::Blob { mapped: Some(m), .. } if overlaps(m.offset, m.len)));
        let map_info = match fail.or(taken.then_some(cmd::RESP_ERR_INVALID_PARAMETER)) {
            Some(code) => Err(code),
            None => backend.map_info(id).map_err(|_| cmd::RESP_ERR_UNSPEC),
        };
        let map_info = match map_info {
            Ok(m) => m,
            Err(code) => {
                let _ = backend.resource_unmap(id);
                return Err(code);
            }
        };
        let Some(Resource { kind: Kind::Blob { mapped, .. }, .. }) = self.resources.get_mut(&id) else { unreachable!() };
        *mapped = Some(Mapped { offset, len, leaving: false });
        self.deferred = Some(Deferred::Map { resource_id: id, offset, ptr, len });
        Ok(Response::OkMapInfo { map_info })
    }

    /// device.rs couldn't map a blob MAP_BLOB asked for (its reply then
    /// says so): the renderer lets go of the memory.
    pub fn map_failed(&mut self, id: u32) {
        if let Some(Resource { kind: Kind::Blob { mapped, .. }, .. }) = self.resources.get_mut(&id) {
            *mapped = None;
            let _ = self.backend.as_mut().unwrap().resource_unmap(id);
        }
    }

    /// device.rs took a blob out of the region (UNMAP_BLOB, or a reset):
    /// only now may the renderer free its memory.
    pub fn unmapped(&mut self, id: u32) {
        let Some(backend) = self.backend.as_mut() else { return };
        if let Some(backing) = self.orphans.remove(&id) {
            let _ = backend.resource_unmap(id);
            backend.resource_unref(id);
            drop(backing);
        } else if let Some(Resource { kind: Kind::Blob { mapped, .. }, .. }) = self.resources.get_mut(&id) {
            *mapped = None;
            let _ = backend.resource_unmap(id);
        }
    }

    /// The host copy of `rect` of a resource as BGRA, rows `stride` apart:
    /// for the display's stream of resources (frames.rs).
    #[allow(dead_code)]
    pub fn read_resource(&mut self, id: u32, rect: Rect, dst: &mut [u8], stride: usize) -> Result<(), u32> {
        let res = self.resources.get(&id).ok_or(cmd::RESP_ERR_INVALID_RESOURCE_ID)?;
        let need = if rect.is_empty() { 0 } else { (rect.height as usize - 1) * stride + rect.width as usize * 4 };
        if !rect.fits(res.width, res.height) || res.format.is_none() || stride < rect.width as usize * 4 || dst.len() < need {
            return Err(cmd::RESP_ERR_INVALID_PARAMETER);
        }
        self.refresh(id, rect)?;
        copy_rows(&self.resources[&id], rect, dst, stride);
        Ok(())
    }

    /// The guest reset the device: everything goes. Mapped blobs stay in
    /// the renderer until device.rs has taken them out of the region (the
    /// Unmap work returned, answered with `unmapped`).
    pub fn reset(&mut self) -> Vec<Deferred> {
        let mut unmaps = Vec::new();
        for (id, res) in self.resources.drain() {
            if let Kind::Blob { mapped: Some(m), .. } = res.kind {
                self.orphans.insert(id, res.backing);
                // One UNMAP_BLOB already asked for is on its way out: its
                // completion finishes it, and a second unmap could land on
                // a later mapping at the same offset.
                if !m.leaving {
                    unmaps.push(Deferred::Unmap { resource_id: id, offset: m.offset, len: m.len });
                }
            } else if res.in_renderer() {
                self.backend.as_mut().unwrap().resource_unref(id);
            }
        }
        for (ctx, _) in self.contexts.drain() {
            self.backend.as_mut().unwrap().ctx_destroy(ctx);
        }
        // device.rs answers every waiting request on a reset anyway.
        self.destroyed.clear();
        self.scanouts.iter_mut().for_each(|s| *s = None);
        self.cursor = None;
        self.host_bytes = 0;
        self.gpu_bytes = 0;
        self.frames.clear();
        unmaps
    }

    /// The state a VM snapshot needs: resources and where their backing is
    /// in guest memory (which the snapshot holds), scanouts and the cursor.
    /// Host pixels aren't saved; they're read back from the backing. What
    /// lives in the renderer can't be saved: while any of it exists this is
    /// an error, and the VM is saved to disk instead.
    pub fn save_state(&self) -> Result<Vec<u8>, &'static str> {
        if !self.contexts.is_empty() || self.resources.values().any(Resource::in_renderer) {
            return Err("3D contexts and resources can't be saved");
        }
        let mut resources: Vec<SavedResource> = self
            .resources
            .iter()
            .map(|(&id, r)| SavedResource {
                id,
                format: r.format.map_or(0, |f| f as u32),
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
        Ok(serde_json::to_vec(&saved).unwrap())
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

/// Rows of `rect` of a resource's host copy into `dst` as BGRA, top row
/// first, rows `stride` apart.
fn copy_rows(res: &Resource, rect: Rect, dst: &mut [u8], stride: usize) {
    let (n, src_stride, format) = (rect.width as usize * 4, res.stride() as usize, res.format.unwrap());
    for r in 0..rect.height as usize {
        let s = (rect.y as usize + r) * src_stride + rect.x as usize * 4;
        convert(&res.pixels[s..s + n], &mut dst[r * stride..r * stride + n], format);
    }
}

/// Draw `dirty` (resource coordinates, inside the binding's rect) of a
/// scanout's resource into its frame.
fn publish(frames: &Frames, scanout: u32, b: Binding, res: &Resource, dirty: Rect) {
    let into = Rect::new(dirty.x - b.rect.x, dirty.y - b.rect.y, dirty.width, dirty.height);
    frames.update(scanout, b.rect.width, b.rect.height, into, |px, stride| {
        let at = into.y as usize * stride + into.x as usize * 4;
        copy_rows(res, dirty, &mut px[at..], stride);
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
        let (hdr, resp) = Response::parse(s.handle(&req.serialize(), mem).bytes()).unwrap();
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
        let (_, r) = Response::parse(s.handle(&[1, 2, 3], &ram).bytes()).unwrap();
        assert_eq!(r, Response::Err(cmd::RESP_ERR_UNSPEC));
        // Unknown command.
        let mut b = vec![0u8; 24];
        b[..4].copy_from_slice(&0x0777u32.to_le_bytes());
        assert_eq!(Response::parse(s.handle(&b, &ram).bytes()).unwrap().1, Response::Err(cmd::RESP_ERR_UNSPEC));
        // Too many entries.
        let mut a = Request { hdr: CtrlHdr::default(), cmd: Command::ResourceAttachBacking { resource_id: 1, entries: vec![] } }.serialize();
        a[28..32].copy_from_slice(&(MAX_ENTRIES + 1).to_le_bytes());
        assert_eq!(Response::parse(s.handle(&a, &ram).bytes()).unwrap().1, Response::Err(cmd::RESP_ERR_OUT_OF_MEMORY));
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
        let state = s.save_state().unwrap();

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

    mod three_d {
        //! The 3D routing, against the fake renderer.

        use std::sync::{Arc, Mutex};

        use super::*;
        use crate::gpu::backend::fake::{Call, FakeBackend, Shared, calls};
        use crate::gpu::protocol::{Box3d, F_CONTEXT_INIT, FLAG_FENCE, FLAG_INFO_RING_IDX, blob_flags, blob_mem, map_cache};

        const R8G8B8A8: u32 = 67;
        const R8G8B8X8: u32 = 134;

        fn gpu(f: &'static Frames) -> (Server, Arc<Mutex<Shared>>) {
            let (b, shared) = FakeBackend::new();
            (Server::new(vec![(64, 32)], vec![], 64 << 20, f).with_backend(Box::new(b)), shared)
        }

        fn on(ctx: u32) -> CtrlHdr {
            CtrlHdr { ctx_id: ctx, ..Default::default() }
        }

        fn fenced(ctx: u32, fence_id: u64) -> CtrlHdr {
            CtrlHdr { flags: FLAG_FENCE, fence_id, ctx_id: ctx, ..Default::default() }
        }

        fn send(s: &mut Server, mem: &dyn GuestMemory, hdr: CtrlHdr, cmd: Command) -> Reply {
            s.handle(&Request { hdr, cmd }.serialize(), mem)
        }

        /// The response, of a reply answered now.
        fn now(s: &mut Server, mem: &dyn GuestMemory, hdr: CtrlHdr, cmd: Command) -> Response {
            match send(s, mem, hdr, cmd) {
                Reply::Now(b) => Response::parse(&b).unwrap().1,
                _ => panic!("not answered now"),
            }
        }

        // Unfenced, so they're answered now even with a renderer.
        fn ok(s: &mut Server, mem: &dyn GuestMemory, cmd: Command) {
            let c = format!("{cmd:?}");
            assert_eq!(now(s, mem, on(0), cmd), Response::OkNoData, "{c}");
        }

        fn err(s: &mut Server, mem: &dyn GuestMemory, cmd: Command) -> u32 {
            match now(s, mem, on(0), cmd) {
                Response::Err(code) => code,
                other => panic!("expected an error, got {other:?}"),
            }
        }

        fn texture(id: u32, format: u32, w: u32, h: u32, flags: u32) -> Command {
            Command::ResourceCreate3d(Create3d {
                resource_id: id,
                target: 2,
                format,
                bind: 2,
                width: w,
                height: h,
                depth: 1,
                array_size: 1,
                flags,
                ..Default::default()
            })
        }

        fn ctx(s: &mut Server, ram: &Ram, id: u32) {
            assert_eq!(now(s, ram, on(id), Command::CtxCreate { context_init: 0, debug_name: b"t".to_vec() }), Response::OkNoData);
        }

        #[test]
        fn contexts() {
            let ram = Ram::new(BASE, 4096);
            let (mut s, shared) = gpu(frames());
            let create = |init| Command::CtxCreate { context_init: init, debug_name: b"weston".to_vec() };
            assert_eq!(now(&mut s, &ram, on(1), create(0)), Response::OkNoData);
            assert_eq!(now(&mut s, &ram, on(1), create(0)), Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID), "twice");
            assert_eq!(now(&mut s, &ram, on(0), create(0)), Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID), "ctx 0");
            // Without CONTEXT_INIT the capset asked for is ignored: VIRGL2.
            assert_eq!(now(&mut s, &ram, on(2), create(capset::VENUS)), Response::OkNoData);
            // With it, it's used, and must be one offered.
            s.set_negotiated(F_CONTEXT_INIT);
            assert_eq!(now(&mut s, &ram, on(3), create(capset::VENUS)), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER));
            assert_eq!(now(&mut s, &ram, on(3), create(capset::VIRGL2)), Response::OkNoData);
            assert_eq!(
                calls(&shared),
                vec![
                    Call::CtxCreate(1, capset::VIRGL2, b"weston".to_vec()),
                    Call::CtxCreate(2, capset::VIRGL2, b"weston".to_vec()),
                    Call::CtxCreate(3, capset::VIRGL2, b"weston".to_vec()),
                ]
            );
            assert_eq!(now(&mut s, &ram, on(2), Command::CtxDestroy), Response::OkNoData);
            assert_eq!(now(&mut s, &ram, on(2), Command::CtxDestroy), Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID));
            assert_eq!(now(&mut s, &ram, on(9), Command::Submit3d { data: vec![0; 8] }), Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID));
            assert_eq!(now(&mut s, &ram, on(1), Command::Submit3d { data: vec![0; 6] }), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER), "not words");
            assert_eq!(now(&mut s, &ram, on(1), Command::Submit3d { data: vec![1, 0, 0, 0] }), Response::OkNoData);
            assert_eq!(now(&mut s, &ram, on(9), Command::CtxAttachResource { resource_id: 1 }), Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID));
            assert_eq!(now(&mut s, &ram, on(1), Command::CtxAttachResource { resource_id: 1 }), Response::Err(cmd::RESP_ERR_INVALID_RESOURCE_ID));
            assert_eq!(calls(&shared), vec![Call::CtxDestroy(2), Call::Submit(1, vec![1, 0, 0, 0])]);
            // 2D resources are only recorded: the renderer doesn't know them.
            ok(&mut s, &ram, Command::ResourceCreate2d { resource_id: 5, format: 2, width: 8, height: 8 });
            calls(&shared);
            assert_eq!(now(&mut s, &ram, on(1), Command::CtxAttachResource { resource_id: 5 }), Response::OkNoData);
            assert!(calls(&shared).is_empty());
            assert!(s.contexts[&1].resources.contains(&5));
        }

        #[test]
        fn resources_and_backing_reach_the_renderer() {
            let ram = Ram::new(BASE, 1 << 20);
            let (mut s, shared) = gpu(frames());
            ctx(&mut s, &ram, 1);
            calls(&shared);
            assert_eq!(now(&mut s, &ram, on(1), texture(7, 1, 64, 64, 0)), Response::OkNoData);
            assert_eq!(now(&mut s, &ram, on(1), texture(7, 1, 64, 64, 0)), Response::Err(cmd::RESP_ERR_INVALID_RESOURCE_ID));
            assert_eq!(now(&mut s, &ram, on(1), texture(8, 1, MAX_DIMENSION + 1, 1, 0)), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER));
            assert_eq!(now(&mut s, &ram, on(1), texture(8, 1, 16384, 16384, 0)), Response::Err(cmd::RESP_ERR_OUT_OF_MEMORY), "a GiB");
            // Four contiguous pages and one apart: two iovecs, the first merged.
            let mut entries: Vec<MemEntry> = (0..4).map(|p| MemEntry { addr: BASE + p * 4096, length: 4096 }).collect();
            entries.push(MemEntry { addr: BASE + 8 * 4096, length: 4096 });
            ok(&mut s, &ram, Command::ResourceAttachBacking { resource_id: 7, entries });
            let base = ram.bytes.borrow().as_ptr() as usize;
            ok(&mut s, &ram, Command::ResourceDetachBacking { resource_id: 7 });
            let c = calls(&shared);
            let Call::Create3d(args) = &c[0] else { panic!() };
            assert_eq!((args.resource_id, args.width), (7, 64));
            assert_eq!(&c[1..], &[Call::AttachIov(7, vec![(base, 4 * 4096), (base + 8 * 4096, 4096)]), Call::DetachIov(7)]);
            // 2D commands that don't apply to 3D resources say so.
            assert_eq!(
                err(&mut s, &ram, Command::TransferToHost2d { rect: Rect::new(0, 0, 1, 1), offset: 0, resource_id: 7 }),
                cmd::RESP_ERR_INVALID_PARAMETER
            );
            let t = Transfer3d { box_: Box3d { w: 1, h: 1, d: 1, ..Default::default() }, resource_id: 7, ..Default::default() };
            assert_eq!(now(&mut s, &ram, on(1), Command::TransferToHost3d(t)), Response::OkNoData);
            assert_eq!(now(&mut s, &ram, on(1), Command::TransferFromHost3d(t)), Response::OkNoData);
            assert_eq!(now(&mut s, &ram, on(4), Command::TransferToHost3d(t)), Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID));
            let missing = Transfer3d { resource_id: 99, ..t };
            assert_eq!(now(&mut s, &ram, on(1), Command::TransferToHost3d(missing)), Response::Err(cmd::RESP_ERR_INVALID_RESOURCE_ID));
            assert_eq!(calls(&shared), vec![Call::ToHost(1, t), Call::FromHost(1, t)]);
        }

        #[test]
        fn fenced_requests_wait_for_the_renderer() {
            let ram = Ram::new(BASE, 4096);
            let (mut s, shared) = gpu(frames());
            ctx(&mut s, &ram, 3);
            calls(&shared);
            // Unfenced: answered now.
            assert!(matches!(send(&mut s, &ram, on(3), Command::Submit3d { data: vec![] }), Reply::Now(_)));
            // Fenced: a host sequence for the renderer, the guest's id in the reply.
            let big = (1u64 << 32) + 5;
            let Reply::Fenced { on: first, bytes } = send(&mut s, &ram, fenced(3, big), Command::Submit3d { data: vec![] }) else { panic!() };
            let (hdr, resp) = Response::parse(&bytes).unwrap();
            assert_eq!((resp, hdr.fence_id, hdr.ctx_id, hdr.flags), (Response::OkNoData, big, 3, FLAG_FENCE));
            let Reply::Fenced { on: second, .. } = send(&mut s, &ram, fenced(3, big + 1), Command::GetDisplayInfo) else { panic!() };
            let (Signal::Global(a), Signal::Global(b)) = (first, second) else { panic!() };
            assert_eq!(b, a + 1, "2D requests share the timeline");
            assert_eq!(calls(&shared), vec![Call::Submit(3, vec![]), Call::Submit(3, vec![]), Call::Fence(a), Call::Fence(b)]);
            // A failed request is fenced too, so it can't overtake.
            let Reply::Fenced { bytes, .. } = send(&mut s, &ram, fenced(9, big + 2), Command::Submit3d { data: vec![] }) else { panic!() };
            assert_eq!(Response::parse(&bytes).unwrap().1, Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID));
            // Ring fences: only with CONTEXT_INIT, keyed by the guest's id.
            let ring = CtrlHdr { flags: FLAG_FENCE | FLAG_INFO_RING_IDX, fence_id: big, ctx_id: 3, ring_idx: 2, ..Default::default() };
            assert!(matches!(send(&mut s, &ram, ring, Command::GetDisplayInfo), Reply::Fenced { on: Signal::Global(_), .. }));
            s.set_negotiated(F_CONTEXT_INIT);
            let Reply::Fenced { on, bytes } = send(&mut s, &ram, ring, Command::Submit3d { data: vec![] }) else { panic!() };
            assert_eq!(on, Signal::Ring { ctx: 3, ring: 2, fence: big });
            let (hdr, _) = Response::parse(&bytes).unwrap();
            assert_eq!((hdr.fence_id, hdr.ctx_id, hdr.ring_idx, hdr.flags), (big, 3, 2, FLAG_FENCE | FLAG_INFO_RING_IDX));
            assert_eq!(calls(&shared).last(), Some(&Call::ContextFence(3, 2, big)));
            // A ring fence on a context that doesn't exist can't be made.
            assert!(matches!(send(&mut s, &ram, CtrlHdr { ctx_id: 8, ..ring }, Command::GetDisplayInfo), Reply::Now(_)));
            // No renderer: everything is answered now, as before.
            let mut plain = server();
            assert!(matches!(send(&mut plain, &ram, fenced(0, 1), Command::GetDisplayInfo), Reply::Now(_)));
        }

        #[test]
        fn host_sequence_skips_zero_when_it_wraps() {
            let ram = Ram::new(BASE, 4096);
            let (mut s, _) = gpu(frames());
            s.fence_seq = u32::MAX;
            let Reply::Fenced { on, .. } = send(&mut s, &ram, fenced(0, 1), Command::GetDisplayInfo) else { panic!() };
            assert_eq!(on, Signal::Global(1));
        }

        #[test]
        fn unref_reaches_the_renderer_and_unbinds() {
            let ram = Ram::new(BASE, 1 << 20);
            let f = frames();
            let (mut s, shared) = gpu(f);
            ctx(&mut s, &ram, 1);
            ctx(&mut s, &ram, 2);
            now(&mut s, &ram, on(1), texture(7, 2, 64, 32, 1));
            now(&mut s, &ram, on(1), Command::CtxAttachResource { resource_id: 7 });
            now(&mut s, &ram, on(2), Command::CtxAttachResource { resource_id: 7 });
            ok(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, 64, 32), scanout_id: 0, resource_id: 7 });
            assert!(f.latest(0).is_some());
            calls(&shared);
            ok(&mut s, &ram, Command::ResourceUnref { resource_id: 7 });
            assert_eq!(calls(&shared), vec![Call::Unref(7)]);
            assert!(f.latest(0).is_none(), "the scanout is off");
            assert!(s.contexts.values().all(|c| !c.resources.contains(&7)));
            assert_eq!((s.gpu_bytes, s.host_bytes), (0, 0));
        }

        #[test]
        fn shown_3d_resources_are_read_back() {
            let ram = Ram::new(BASE, 4096);
            let f = frames();
            let (mut s, shared) = gpu(f);
            // What the renderer holds: its pixel (x, y) in its own rows, RGBA.
            shared.lock().unwrap().pixel = Some(|x, y| [x as u8, y as u8, 0x10, 0x20]);
            ctx(&mut s, &ram, 1);
            // No Y_0_TOP: the readback is top-down all the same.
            now(&mut s, &ram, on(1), texture(7, R8G8B8A8, 64, 32, 0));
            calls(&shared);
            ok(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, 64, 32), scanout_id: 0, resource_id: 7 });
            assert_eq!(calls(&shared), vec![Call::ReadBack(7, Rect::new(0, 0, 64, 32), 256)]);
            let frame = f.latest(0).unwrap();
            let at = |fr: &frames::Frame, x: u32, y: u32| fr.pixels[((y * fr.width + x) * 4) as usize..][..4].to_vec();
            // RGBA comes out BGRA.
            assert_eq!(at(&frame, 3, 2), [0x10, 2, 3, 0x20]);
            assert_eq!(at(&frame, 63, 31), [0x10, 31, 63, 0x20]);
            // A flush reads back only what it covers.
            ok(&mut s, &ram, Command::ResourceFlush { rect: Rect::new(10, 4, 8, 6), resource_id: 7 });
            assert_eq!(calls(&shared), vec![Call::ReadBack(7, Rect::new(10, 4, 8, 6), 256)]);
            assert!(f.latest(0).unwrap().sequence > frame.sequence);

            // Top-down, without alpha, shown in part: the flush is cut to it.
            now(&mut s, &ram, on(1), texture(8, R8G8B8X8, 64, 32, 1));
            ok(&mut s, &ram, Command::SetScanout { rect: Rect::new(8, 8, 16, 8), scanout_id: 0, resource_id: 8 });
            calls(&shared);
            ok(&mut s, &ram, Command::ResourceFlush { rect: Rect::new(0, 0, 64, 32), resource_id: 8 });
            assert_eq!(calls(&shared), vec![Call::ReadBack(8, Rect::new(8, 8, 16, 8), 256)]);
            let part = f.latest(0).unwrap();
            assert_eq!((part.width, part.height), (16, 8));
            assert_eq!(at(&part, 1, 1), [0x10, 9, 9, 0xff]);
            // The host copy counts against the host limit, and goes with it.
            assert_eq!(s.host_bytes, 2 * (64 * 32 * 4 + 256));

            // Formats the display can't show, and buffers, can't be scanned out.
            now(&mut s, &ram, on(1), texture(9, 5, 64, 32, 1));
            assert_eq!(err(&mut s, &ram, Command::SetScanout { rect: Rect::new(0, 0, 8, 8), scanout_id: 0, resource_id: 9 }), cmd::RESP_ERR_INVALID_PARAMETER);

            // The stream of resources gets top-down BGRA.
            let mut px = vec![0u8; 2 * 8];
            s.read_resource(7, Rect::new(4, 30, 2, 2), &mut px, 8).unwrap();
            assert_eq!(&px, &[0x10, 30, 4, 0x20, 0x10, 30, 5, 0x20, 0x10, 31, 4, 0x20, 0x10, 31, 5, 0x20]);
            assert_eq!(s.read_resource(7, Rect::new(63, 0, 2, 1), &mut px, 8), Err(cmd::RESP_ERR_INVALID_PARAMETER));
        }

        #[test]
        fn resource_records_reach_the_renderer() {
            use crate::gpu::frames::{SurfaceKey, Surfaces};
            // The display's reader is a plain fn, as device::read_resource
            // is; the feeder calls it on this thread.
            thread_local! { static GPU: RefCell<Option<Server>> = const { RefCell::new(None) }; }
            fn reader(id: u32, rect: Rect, dst: &mut [u8], stride: usize) -> anyhow::Result<()> {
                GPU.with_borrow_mut(|s| s.as_mut().unwrap().read_resource(id, rect, dst, stride))
                    .map_err(|code| anyhow::anyhow!("error {code:#x}"))
            }
            let ram = Ram::new(BASE, 4096);
            let (mut s, shared) = gpu(frames());
            shared.lock().unwrap().pixel = Some(|x, y| [x as u8, y as u8, 0x10, 0x20]);
            ctx(&mut s, &ram, 1);
            now(&mut s, &ram, on(1), texture(7, R8G8B8A8, 64, 32, 1));
            calls(&shared);
            GPU.set(Some(s));

            let surfaces: &'static Surfaces = Box::leak(Box::new(Surfaces::new()));
            let key = SurfaceKey::Session { machine: "dev".into(), session: "gl".into() };
            let rec = b"{\"format\":\"resource\",\"res_id\":7,\"w\":64,\"h\":32,\"damage\":[[0,0,64,32]],\"payload_len\":0}\n";
            crate::gui::display::feed_with(&mut &rec[..], surfaces, &key, Some(reader)).unwrap();
            assert!(matches!(calls(&shared)[..], [Call::ReadBack(7, r, _)] if r == Rect::new(0, 0, 64, 32)));
            let frame = surfaces.frame(&key).unwrap();
            assert_eq!((frame.width, frame.height), (64, 32));
            // RGBA from the renderer, BGRA in the surface, top row first.
            assert_eq!(frame.pixels[(2 * 64 + 3) * 4..][..4], [0x10, 2, 3, 0x20]);
            // An unknown resource ends the stream.
            let rec = b"{\"format\":\"resource\",\"res_id\":9,\"damage\":[[0,0,1,1]],\"payload_len\":0}\n";
            let err = crate::gui::display::feed_with(&mut &rec[..], surfaces, &key, Some(reader)).unwrap_err();
            assert!(err.contains("GPU resource 9"), "{err}");
        }

        #[test]
        fn saving_fails_while_3d_exists() {
            let ram = Ram::new(BASE, 4096);
            let (mut s, _) = gpu(frames());
            assert!(s.save_state().is_ok());
            ctx(&mut s, &ram, 1);
            assert!(s.save_state().is_err());
            now(&mut s, &ram, on(1), Command::CtxDestroy);
            now(&mut s, &ram, on(1), texture(7, 1, 8, 8, 0));
            assert!(s.save_state().is_err());
            ok(&mut s, &ram, Command::ResourceUnref { resource_id: 7 });
            assert!(s.save_state().is_ok());
        }

        #[test]
        fn reset_destroys_contexts_and_resources() {
            let ram = Ram::new(BASE, 4096);
            let (mut s, shared) = gpu(frames());
            ctx(&mut s, &ram, 1);
            now(&mut s, &ram, on(1), texture(7, 1, 8, 8, 0));
            ok(&mut s, &ram, Command::ResourceCreate2d { resource_id: 8, format: 2, width: 8, height: 8 });
            calls(&shared);
            assert!(s.reset().is_empty());
            assert_eq!(calls(&shared), vec![Call::Unref(7), Call::CtxDestroy(1)]);
            assert!(s.contexts.is_empty() && s.resources.is_empty());
        }

        #[test]
        fn blobs_only_with_gpu_blob() {
            let ram = Ram::new(BASE, 1 << 20);
            let blob = |id, mem, flags| Command::ResourceCreateBlob { resource_id: id, blob_mem: mem, blob_flags: flags, blob_id: 1, size: 65536, entries: vec![] };
            let (mut s, _) = gpu(frames());
            ctx(&mut s, &ram, 1);
            assert_eq!(now(&mut s, &ram, on(1), blob(5, blob_mem::HOST3D, blob_flags::USE_MAPPABLE)), Response::Err(cmd::RESP_ERR_UNSPEC));
            assert_eq!(now(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 5, offset: 0 }), Response::Err(cmd::RESP_ERR_UNSPEC));

            let (s, shared) = gpu(frames());
            let mut s = s.with_blobs(1 << 28);
            shared.lock().unwrap().map = Some((0x1_0000_0000, 65536));
            ctx(&mut s, &ram, 1);
            calls(&shared);
            assert_eq!(now(&mut s, &ram, on(1), blob(5, blob_mem::HOST3D, blob_flags::USE_MAPPABLE)), Response::OkNoData);
            assert_eq!(now(&mut s, &ram, on(2), blob(6, blob_mem::HOST3D, 0)), Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID));
            assert_eq!(now(&mut s, &ram, on(1), blob(6, blob_mem::HOST3D, blob_flags::USE_MAPPABLE)), Response::OkNoData);
            assert_eq!(now(&mut s, &ram, on(1), blob(7, blob_mem::HOST3D, 0)), Response::OkNoData);
            // Mapping: answered once the region has it, at a page-aligned offset.
            let Reply::Deferred { op, bytes } = send(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 5, offset: 0x4000 }) else { panic!() };
            assert_eq!(op, Deferred::Map { resource_id: 5, offset: 0x4000, ptr: 0x1_0000_0000 as *mut u8, len: 65536 });
            assert_eq!(Response::parse(&bytes).unwrap().1, Response::OkMapInfo { map_info: map_cache::CACHED });
            assert_eq!(now(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 6, offset: 0x1000 }), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER), "misaligned");
            assert_eq!(now(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 6, offset: 1 << 28 }), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER), "past the region");
            assert_eq!(now(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 5, offset: 0x40000 }), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER), "mapped already");
            // Not mappable: the renderer isn't asked.
            calls(&shared);
            assert_eq!(now(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 7, offset: 0x40000 }), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER));
            assert!(calls(&shared).is_empty());
            // Over another mapping: refused, and the renderer lets go again.
            assert_eq!(now(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 6, offset: 0x8000 }), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER), "overlaps");
            assert_eq!(calls(&shared), vec![Call::Map(6), Call::Unmap(6)]);
            // A mapped blob can't go before it's out of the region.
            assert_eq!(err(&mut s, &ram, Command::ResourceUnref { resource_id: 5 }), cmd::RESP_ERR_UNSPEC);
            let Reply::Deferred { op, .. } = send(&mut s, &ram, on(1), Command::ResourceUnmapBlob { resource_id: 5 }) else { panic!() };
            assert_eq!(op, Deferred::Unmap { resource_id: 5, offset: 0x4000, len: 65536 });
            s.unmapped(5);
            ok(&mut s, &ram, Command::ResourceUnref { resource_id: 5 });
            assert_eq!(calls(&shared), vec![Call::Unmap(5), Call::Unref(5)]);

            // A reset leaves mapped blobs to be taken out of the region first.
            assert_eq!(now(&mut s, &ram, on(1), blob(9, blob_mem::HOST3D, blob_flags::USE_MAPPABLE)), Response::OkNoData);
            assert!(matches!(send(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 9, offset: 0 }), Reply::Deferred { .. }));
            calls(&shared);
            assert_eq!(s.reset(), vec![Deferred::Unmap { resource_id: 9, offset: 0, len: 65536 }]);
            assert_eq!(now(&mut s, &ram, on(0), Command::ResourceCreate2d { resource_id: 9, format: 2, width: 8, height: 8 }), Response::Err(cmd::RESP_ERR_INVALID_RESOURCE_ID));
            s.unmapped(9);
            assert!(calls(&shared).ends_with(&[Call::Unmap(9), Call::Unref(9)]));

            // A reset while an UNMAP_BLOB is on its way out doesn't ask for
            // a second one: that unmap's completion finishes the blob.
            ctx(&mut s, &ram, 1);
            assert_eq!(now(&mut s, &ram, on(1), blob(11, blob_mem::HOST3D, blob_flags::USE_MAPPABLE)), Response::OkNoData);
            assert!(matches!(send(&mut s, &ram, on(1), Command::ResourceMapBlob { resource_id: 11, offset: 0 }), Reply::Deferred { .. }));
            assert!(matches!(send(&mut s, &ram, on(1), Command::ResourceUnmapBlob { resource_id: 11 }), Reply::Deferred { .. }));
            calls(&shared);
            assert!(s.reset().is_empty());
            assert_eq!(calls(&shared), vec![Call::CtxDestroy(1)], "the blob stays in the renderer");
            s.unmapped(11);
            assert_eq!(calls(&shared), vec![Call::Unmap(11), Call::Unref(11)]);

            // Guest blobs need their memory, as much as they claim.
            let guest = |entries| Command::ResourceCreateBlob { resource_id: 10, blob_mem: blob_mem::GUEST, blob_flags: 0, blob_id: 0, size: 8192, entries };
            assert_eq!(now(&mut s, &ram, on(0), guest(vec![MemEntry { addr: BASE, length: 4096 }])), Response::Err(cmd::RESP_ERR_INVALID_PARAMETER));
            assert_eq!(now(&mut s, &ram, on(0), guest(vec![MemEntry { addr: BASE, length: 8192 }])), Response::OkNoData);
        }

        #[test]
        fn guest_blobs_scan_out_from_their_memory() {
            let ram = Ram::new(BASE, 1 << 20);
            let f = frames();
            let (s, _) = gpu(f);
            let mut s = s.with_blobs(1 << 28);
            // 16x4, rows 128 bytes apart (wider than 64), starting at 256.
            let entries = vec![MemEntry { addr: BASE, length: 4096 }];
            let blob = Command::ResourceCreateBlob { resource_id: 3, blob_mem: blob_mem::GUEST, blob_flags: 0, blob_id: 0, size: 4096, entries };
            assert_eq!(now(&mut s, &ram, on(0), blob), Response::OkNoData);
            ram.write(BASE + 256 + 2 * 128 + 5 * 4, &[1, 2, 3, 4]);
            let show = |offset| Command::SetScanoutBlob {
                rect: Rect::new(0, 0, 16, 4),
                scanout_id: 0,
                resource_id: 3,
                width: 16,
                height: 4,
                format: Format::B8G8R8X8 as u32,
                strides: [128, 0, 0, 0],
                offsets: [offset, 0, 0, 0],
            };
            ok(&mut s, &ram, show(256));
            let frame = f.latest(0).unwrap();
            assert_eq!(&frame.pixels[((2 * 16 + 5) * 4)..][..4], &[1, 2, 3, 0xff]);
            assert_eq!(err(&mut s, &ram, show(4000)), cmd::RESP_ERR_INVALID_PARAMETER, "past the blob");
        }

        #[test]
        fn submit_on_an_unknown_context() {
            let ram = Ram::new(BASE, 4096);
            let (mut s, _) = gpu(frames());
            assert_eq!(now(&mut s, &ram, on(5), Command::Submit3d { data: vec![0; 16] }), Response::Err(cmd::RESP_ERR_INVALID_CONTEXT_ID));
        }
    }
}
