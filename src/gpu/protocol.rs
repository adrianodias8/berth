//! The virtio-gpu wire format (virtio 1.2/1.3, section "GPU Device"): the
//! control header, every 2D, 3D, blob and cursor command, the responses,
//! display info, EDID, and the device configuration space. Pure data: no
//! I/O, so all of it is unit tested here. The command server is server.rs.
//!
//! Everything is little-endian. Requests come from the guest and are
//! untrusted: parsing checks every length, and a command that's short or
//! carries more scatter-gather entries than we accept is an error, never a
//! panic or an unbounded allocation.

// The whole format is here, including what only the 3D path, spikes and
// tests use yet.
#![allow(dead_code)]

use std::fmt;

/// virtio device ID of a GPU.
pub const VIRTIO_ID_GPU: u16 = 16;

/// Feature bits (subset 0).
pub const F_VIRGL: u32 = 1 << 0;
pub const F_EDID: u32 = 1 << 1;
pub const F_RESOURCE_UUID: u32 = 1 << 2;
pub const F_RESOURCE_BLOB: u32 = 1 << 3;
pub const F_CONTEXT_INIT: u32 = 1 << 4;

/// `events_read` / `events_clear`: the display configuration changed.
pub const EVENT_DISPLAY: u32 = 1 << 0;

/// Most scanouts the protocol describes (display info has room for 16).
pub const MAX_SCANOUTS: usize = 16;

/// Shared memory region ID of the host-visible region (blob mappings).
pub const SHM_ID_HOST_VISIBLE: u8 = 1;

/// Header flags.
pub const FLAG_FENCE: u32 = 1 << 0;
pub const FLAG_INFO_RING_IDX: u32 = 1 << 1;

/// Command and response types.
pub mod cmd {
    // 2D
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
    pub const GET_CAPSET_INFO: u32 = 0x0108;
    pub const GET_CAPSET: u32 = 0x0109;
    pub const GET_EDID: u32 = 0x010a;
    pub const RESOURCE_ASSIGN_UUID: u32 = 0x010b;
    pub const RESOURCE_CREATE_BLOB: u32 = 0x010c;
    pub const SET_SCANOUT_BLOB: u32 = 0x010d;
    // 3D
    pub const CTX_CREATE: u32 = 0x0200;
    pub const CTX_DESTROY: u32 = 0x0201;
    pub const CTX_ATTACH_RESOURCE: u32 = 0x0202;
    pub const CTX_DETACH_RESOURCE: u32 = 0x0203;
    pub const RESOURCE_CREATE_3D: u32 = 0x0204;
    pub const TRANSFER_TO_HOST_3D: u32 = 0x0205;
    pub const TRANSFER_FROM_HOST_3D: u32 = 0x0206;
    pub const SUBMIT_3D: u32 = 0x0207;
    pub const RESOURCE_MAP_BLOB: u32 = 0x0208;
    pub const RESOURCE_UNMAP_BLOB: u32 = 0x0209;
    // cursor
    pub const UPDATE_CURSOR: u32 = 0x0300;
    pub const MOVE_CURSOR: u32 = 0x0301;
    // success responses
    pub const RESP_OK_NODATA: u32 = 0x1100;
    pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
    pub const RESP_OK_CAPSET_INFO: u32 = 0x1102;
    pub const RESP_OK_CAPSET: u32 = 0x1103;
    pub const RESP_OK_EDID: u32 = 0x1104;
    pub const RESP_OK_RESOURCE_UUID: u32 = 0x1105;
    pub const RESP_OK_MAP_INFO: u32 = 0x1106;
    // error responses
    pub const RESP_ERR_UNSPEC: u32 = 0x1200;
    pub const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
    pub const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
    pub const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;
}

/// Capability set IDs (GET_CAPSET_INFO).
pub mod capset {
    pub const VIRGL: u32 = 1;
    pub const VIRGL2: u32 = 2;
    pub const GFXSTREAM_VULKAN: u32 = 3;
    pub const VENUS: u32 = 4;
    pub const CROSS_DOMAIN: u32 = 5;
    pub const DRM: u32 = 6;
}

/// `blob_mem` of RESOURCE_CREATE_BLOB.
pub mod blob_mem {
    pub const GUEST: u32 = 1;
    pub const HOST3D: u32 = 2;
    pub const HOST3D_GUEST: u32 = 3;
}

/// `blob_flags` of RESOURCE_CREATE_BLOB.
pub mod blob_flags {
    pub const USE_MAPPABLE: u32 = 1;
    pub const USE_SHAREABLE: u32 = 2;
    pub const USE_CROSS_DEVICE: u32 = 4;
}

/// `map_info` of RESP_OK_MAP_INFO: how the guest maps a blob.
pub mod map_cache {
    pub const MASK: u32 = 0x0f;
    pub const NONE: u32 = 0x00;
    pub const CACHED: u32 = 0x01;
    pub const UNCACHED: u32 = 0x02;
    pub const WC: u32 = 0x03;
}

/// Pixel formats of 2D resources. The names give the byte order in memory
/// (B8G8R8A8: blue first), which is what Linux's XRGB8888 framebuffers are.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[repr(u32)]
pub enum Format {
    B8G8R8A8 = 1,
    B8G8R8X8 = 2,
    A8R8G8B8 = 3,
    X8R8G8B8 = 4,
    R8G8B8A8 = 67,
    X8B8G8R8 = 68,
    A8B8G8R8 = 121,
    R8G8B8X8 = 134,
}

impl Format {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => Format::B8G8R8A8,
            2 => Format::B8G8R8X8,
            3 => Format::A8R8G8B8,
            4 => Format::X8R8G8B8,
            67 => Format::R8G8B8A8,
            68 => Format::X8B8G8R8,
            121 => Format::A8B8G8R8,
            134 => Format::R8G8B8X8,
            _ => return None,
        })
    }

    /// Every format here is 32 bits per pixel.
    pub const BYTES_PER_PIXEL: u32 = 4;

    /// Where blue, green, red and (if it has one) alpha sit in a pixel's
    /// four bytes: what converting to the BGRA the Mac side shows needs.
    pub fn layout(self) -> ([usize; 3], Option<usize>) {
        match self {
            Format::B8G8R8A8 => ([0, 1, 2], Some(3)),
            Format::B8G8R8X8 => ([0, 1, 2], None),
            Format::A8R8G8B8 => ([3, 2, 1], Some(0)),
            Format::X8R8G8B8 => ([3, 2, 1], None),
            Format::R8G8B8A8 => ([2, 1, 0], Some(3)),
            Format::R8G8B8X8 => ([2, 1, 0], None),
            Format::A8B8G8R8 => ([1, 2, 3], Some(0)),
            Format::X8B8G8R8 => ([1, 2, 3], None),
        }
    }
}

/// Why a request couldn't be parsed.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ParseError {
    /// Shorter than its command needs.
    Short { need: usize, have: usize },
    /// A command type we don't know.
    Unknown(u32),
    /// More scatter-gather entries than we take.
    TooManyEntries(u32),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Short { need, have } => write!(f, "request is {have} bytes, needs {need}"),
            ParseError::Unknown(t) => write!(f, "unknown command {t:#06x}"),
            ParseError::TooManyEntries(n) => write!(f, "{n} backing entries is more than {MAX_ENTRIES}"),
        }
    }
}

/// Most scatter-gather entries one command may carry. A 16384x16384
/// resource in 4 KiB pages is 262144 entries, the largest a guest can
/// legitimately send; anything above is a broken or hostile guest.
pub const MAX_ENTRIES: u32 = 1 << 18;

/// Little-endian reader over a request; every read is bounds-checked.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn need(&self, n: usize) -> Result<(), ParseError> {
        match self.pos.checked_add(n) {
            Some(end) if end <= self.buf.len() => Ok(()),
            _ => Err(ParseError::Short { need: self.pos.saturating_add(n), have: self.buf.len() }),
        }
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], ParseError> {
        self.need(n)?;
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }
    fn u8(&mut self) -> Result<u8, ParseError> {
        Ok(self.bytes(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, ParseError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ParseError> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn rest(&mut self) -> &'a [u8] {
        let b = &self.buf[self.pos..];
        self.pos = self.buf.len();
        b
    }
}

/// Little-endian writer.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    fn rect(&mut self, r: Rect) -> &mut Self {
        self.u32(r.x).u32(r.y).u32(r.width).u32(r.height)
    }
}

/// `virtio_gpu_ctrl_hdr`, 24 bytes, at the start of every request and response.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CtrlHdr {
    pub type_: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

impl CtrlHdr {
    pub const SIZE: usize = 24;

    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        let mut r = Reader::new(buf);
        Self::read(&mut r)
    }
    fn read(r: &mut Reader) -> Result<Self, ParseError> {
        let type_ = r.u32()?;
        let flags = r.u32()?;
        let fence_id = r.u64()?;
        let ctx_id = r.u32()?;
        let ring_idx = r.u8()?;
        r.bytes(3)?;
        Ok(Self { type_, flags, fence_id, ctx_id, ring_idx })
    }
    fn write(&self, w: &mut Writer) {
        w.u32(self.type_).u32(self.flags).u64(self.fence_id).u32(self.ctx_id).u8(self.ring_idx).bytes(&[0; 3]);
    }

    /// The header of the response to this request: the device must echo
    /// the fence (and the ring it's on) so the guest can signal it.
    pub fn response(&self, type_: u32) -> Self {
        let fenced = self.flags & FLAG_FENCE != 0;
        let flags = if fenced { self.flags & (FLAG_FENCE | FLAG_INFO_RING_IDX) } else { 0 };
        Self {
            type_,
            flags,
            fence_id: if fenced { self.fence_id } else { 0 },
            ctx_id: self.ctx_id,
            ring_idx: if flags & FLAG_INFO_RING_IDX != 0 { self.ring_idx } else { 0 },
        }
    }
}

/// `virtio_gpu_rect`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }
    fn read(r: &mut Reader) -> Result<Self, ParseError> {
        Ok(Self { x: r.u32()?, y: r.u32()?, width: r.u32()?, height: r.u32()? })
    }
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
    /// Whether the rectangle lies inside a `width` x `height` surface,
    /// without overflowing on hostile values.
    pub fn fits(&self, width: u32, height: u32) -> bool {
        let right = self.x as u64 + self.width as u64;
        let bottom = self.y as u64 + self.height as u64;
        right <= width as u64 && bottom <= height as u64
    }
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x) as u64;
        let y0 = self.y.max(other.y) as u64;
        let x1 = (self.x as u64 + self.width as u64).min(other.x as u64 + other.width as u64);
        let y1 = (self.y as u64 + self.height as u64).min(other.y as u64 + other.height as u64);
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
    }
}

/// `virtio_gpu_mem_entry`: one piece of a resource's guest backing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct MemEntry {
    pub addr: u64,
    pub length: u32,
}

/// `virtio_gpu_box` of the 3D transfers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Box3d {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

/// `virtio_gpu_transfer_host_3d` (both directions).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Transfer3d {
    pub box_: Box3d,
    pub offset: u64,
    pub resource_id: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

/// `virtio_gpu_resource_create_3d`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Create3d {
    pub resource_id: u32,
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
}

/// `virtio_gpu_update_cursor` (UPDATE_CURSOR and MOVE_CURSOR share it).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CursorCmd {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub resource_id: u32,
    pub hot_x: u32,
    pub hot_y: u32,
}

/// One parsed command, without its header.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    GetDisplayInfo,
    ResourceCreate2d { resource_id: u32, format: u32, width: u32, height: u32 },
    ResourceUnref { resource_id: u32 },
    SetScanout { rect: Rect, scanout_id: u32, resource_id: u32 },
    ResourceFlush { rect: Rect, resource_id: u32 },
    TransferToHost2d { rect: Rect, offset: u64, resource_id: u32 },
    ResourceAttachBacking { resource_id: u32, entries: Vec<MemEntry> },
    ResourceDetachBacking { resource_id: u32 },
    GetCapsetInfo { capset_index: u32 },
    GetCapset { capset_id: u32, capset_version: u32 },
    GetEdid { scanout: u32 },
    ResourceAssignUuid { resource_id: u32 },
    ResourceCreateBlob { resource_id: u32, blob_mem: u32, blob_flags: u32, blob_id: u64, size: u64, entries: Vec<MemEntry> },
    SetScanoutBlob {
        rect: Rect,
        scanout_id: u32,
        resource_id: u32,
        width: u32,
        height: u32,
        format: u32,
        strides: [u32; 4],
        offsets: [u32; 4],
    },
    CtxCreate { context_init: u32, debug_name: Vec<u8> },
    CtxDestroy,
    CtxAttachResource { resource_id: u32 },
    CtxDetachResource { resource_id: u32 },
    ResourceCreate3d(Create3d),
    TransferToHost3d(Transfer3d),
    TransferFromHost3d(Transfer3d),
    /// The command stream for the context's renderer (`size` bytes of it).
    Submit3d { data: Vec<u8> },
    ResourceMapBlob { resource_id: u32, offset: u64 },
    ResourceUnmapBlob { resource_id: u32 },
    UpdateCursor(CursorCmd),
    MoveCursor(CursorCmd),
}

/// A request off the control or cursor queue.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    pub hdr: CtrlHdr,
    pub cmd: Command,
}

fn entries(r: &mut Reader, n: u32) -> Result<Vec<MemEntry>, ParseError> {
    if n > MAX_ENTRIES {
        return Err(ParseError::TooManyEntries(n));
    }
    // Checked before allocating: a guest can't make us reserve room for
    // entries it didn't send.
    r.need(n as usize * 16)?;
    (0..n)
        .map(|_| {
            let addr = r.u64()?;
            let length = r.u32()?;
            r.u32()?;
            Ok(MemEntry { addr, length })
        })
        .collect()
}

fn cursor(r: &mut Reader) -> Result<CursorCmd, ParseError> {
    let scanout_id = r.u32()?;
    let x = r.u32()?;
    let y = r.u32()?;
    r.u32()?;
    let resource_id = r.u32()?;
    let hot_x = r.u32()?;
    let hot_y = r.u32()?;
    r.u32()?;
    Ok(CursorCmd { scanout_id, x, y, resource_id, hot_x, hot_y })
}

fn transfer3d(r: &mut Reader) -> Result<Transfer3d, ParseError> {
    let box_ = Box3d { x: r.u32()?, y: r.u32()?, z: r.u32()?, w: r.u32()?, h: r.u32()?, d: r.u32()? };
    Ok(Transfer3d {
        box_,
        offset: r.u64()?,
        resource_id: r.u32()?,
        level: r.u32()?,
        stride: r.u32()?,
        layer_stride: r.u32()?,
    })
}

impl Request {
    /// Parse a request. Extra trailing bytes are ignored, as the spec allows.
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        use cmd::*;
        let mut r = Reader::new(buf);
        let hdr = CtrlHdr::read(&mut r)?;
        let cmd = match hdr.type_ {
            GET_DISPLAY_INFO => Command::GetDisplayInfo,
            RESOURCE_CREATE_2D => {
                Command::ResourceCreate2d { resource_id: r.u32()?, format: r.u32()?, width: r.u32()?, height: r.u32()? }
            }
            RESOURCE_UNREF => {
                let resource_id = r.u32()?;
                r.u32()?;
                Command::ResourceUnref { resource_id }
            }
            SET_SCANOUT => Command::SetScanout { rect: Rect::read(&mut r)?, scanout_id: r.u32()?, resource_id: r.u32()? },
            RESOURCE_FLUSH => {
                let rect = Rect::read(&mut r)?;
                let resource_id = r.u32()?;
                r.u32()?;
                Command::ResourceFlush { rect, resource_id }
            }
            TRANSFER_TO_HOST_2D => {
                let rect = Rect::read(&mut r)?;
                let offset = r.u64()?;
                let resource_id = r.u32()?;
                r.u32()?;
                Command::TransferToHost2d { rect, offset, resource_id }
            }
            RESOURCE_ATTACH_BACKING => {
                let resource_id = r.u32()?;
                let n = r.u32()?;
                Command::ResourceAttachBacking { resource_id, entries: entries(&mut r, n)? }
            }
            RESOURCE_DETACH_BACKING => {
                let resource_id = r.u32()?;
                r.u32()?;
                Command::ResourceDetachBacking { resource_id }
            }
            GET_CAPSET_INFO => {
                let capset_index = r.u32()?;
                r.u32()?;
                Command::GetCapsetInfo { capset_index }
            }
            GET_CAPSET => Command::GetCapset { capset_id: r.u32()?, capset_version: r.u32()? },
            GET_EDID => {
                let scanout = r.u32()?;
                r.u32()?;
                Command::GetEdid { scanout }
            }
            RESOURCE_ASSIGN_UUID => {
                let resource_id = r.u32()?;
                r.u32()?;
                Command::ResourceAssignUuid { resource_id }
            }
            RESOURCE_CREATE_BLOB => {
                let resource_id = r.u32()?;
                let blob_mem = r.u32()?;
                let blob_flags = r.u32()?;
                let n = r.u32()?;
                let blob_id = r.u64()?;
                let size = r.u64()?;
                Command::ResourceCreateBlob { resource_id, blob_mem, blob_flags, blob_id, size, entries: entries(&mut r, n)? }
            }
            SET_SCANOUT_BLOB => {
                let rect = Rect::read(&mut r)?;
                let scanout_id = r.u32()?;
                let resource_id = r.u32()?;
                let width = r.u32()?;
                let height = r.u32()?;
                let format = r.u32()?;
                r.u32()?;
                let mut strides = [0; 4];
                for s in &mut strides {
                    *s = r.u32()?;
                }
                let mut offsets = [0; 4];
                for o in &mut offsets {
                    *o = r.u32()?;
                }
                Command::SetScanoutBlob { rect, scanout_id, resource_id, width, height, format, strides, offsets }
            }
            CTX_CREATE => {
                let nlen = r.u32()?;
                let context_init = r.u32()?;
                let name = r.bytes(64)?;
                Command::CtxCreate { context_init, debug_name: name[..(nlen as usize).min(64)].to_vec() }
            }
            CTX_DESTROY => Command::CtxDestroy,
            CTX_ATTACH_RESOURCE | CTX_DETACH_RESOURCE => {
                let resource_id = r.u32()?;
                r.u32()?;
                if hdr.type_ == CTX_ATTACH_RESOURCE {
                    Command::CtxAttachResource { resource_id }
                } else {
                    Command::CtxDetachResource { resource_id }
                }
            }
            RESOURCE_CREATE_3D => {
                let c = Create3d {
                    resource_id: r.u32()?,
                    target: r.u32()?,
                    format: r.u32()?,
                    bind: r.u32()?,
                    width: r.u32()?,
                    height: r.u32()?,
                    depth: r.u32()?,
                    array_size: r.u32()?,
                    last_level: r.u32()?,
                    nr_samples: r.u32()?,
                    flags: r.u32()?,
                };
                r.u32()?;
                Command::ResourceCreate3d(c)
            }
            TRANSFER_TO_HOST_3D => Command::TransferToHost3d(transfer3d(&mut r)?),
            TRANSFER_FROM_HOST_3D => Command::TransferFromHost3d(transfer3d(&mut r)?),
            SUBMIT_3D => {
                let size = r.u32()?;
                r.u32()?;
                Command::Submit3d { data: r.bytes(size as usize)?.to_vec() }
            }
            RESOURCE_MAP_BLOB => {
                let resource_id = r.u32()?;
                r.u32()?;
                Command::ResourceMapBlob { resource_id, offset: r.u64()? }
            }
            RESOURCE_UNMAP_BLOB => {
                let resource_id = r.u32()?;
                r.u32()?;
                Command::ResourceUnmapBlob { resource_id }
            }
            UPDATE_CURSOR => Command::UpdateCursor(cursor(&mut r)?),
            MOVE_CURSOR => Command::MoveCursor(cursor(&mut r)?),
            other => return Err(ParseError::Unknown(other)),
        };
        let _ = r.rest();
        Ok(Self { hdr, cmd })
    }

    /// The wire form: what a guest driver sends. Used by tests and spikes.
    pub fn serialize(&self) -> Vec<u8> {
        use cmd::*;
        let mut w = Writer::default();
        let mut hdr = self.hdr;
        hdr.type_ = match &self.cmd {
            Command::GetDisplayInfo => GET_DISPLAY_INFO,
            Command::ResourceCreate2d { .. } => RESOURCE_CREATE_2D,
            Command::ResourceUnref { .. } => RESOURCE_UNREF,
            Command::SetScanout { .. } => SET_SCANOUT,
            Command::ResourceFlush { .. } => RESOURCE_FLUSH,
            Command::TransferToHost2d { .. } => TRANSFER_TO_HOST_2D,
            Command::ResourceAttachBacking { .. } => RESOURCE_ATTACH_BACKING,
            Command::ResourceDetachBacking { .. } => RESOURCE_DETACH_BACKING,
            Command::GetCapsetInfo { .. } => GET_CAPSET_INFO,
            Command::GetCapset { .. } => GET_CAPSET,
            Command::GetEdid { .. } => GET_EDID,
            Command::ResourceAssignUuid { .. } => RESOURCE_ASSIGN_UUID,
            Command::ResourceCreateBlob { .. } => RESOURCE_CREATE_BLOB,
            Command::SetScanoutBlob { .. } => SET_SCANOUT_BLOB,
            Command::CtxCreate { .. } => CTX_CREATE,
            Command::CtxDestroy => CTX_DESTROY,
            Command::CtxAttachResource { .. } => CTX_ATTACH_RESOURCE,
            Command::CtxDetachResource { .. } => CTX_DETACH_RESOURCE,
            Command::ResourceCreate3d(_) => RESOURCE_CREATE_3D,
            Command::TransferToHost3d(_) => TRANSFER_TO_HOST_3D,
            Command::TransferFromHost3d(_) => TRANSFER_FROM_HOST_3D,
            Command::Submit3d { .. } => SUBMIT_3D,
            Command::ResourceMapBlob { .. } => RESOURCE_MAP_BLOB,
            Command::ResourceUnmapBlob { .. } => RESOURCE_UNMAP_BLOB,
            Command::UpdateCursor(_) => UPDATE_CURSOR,
            Command::MoveCursor(_) => MOVE_CURSOR,
        };
        hdr.write(&mut w);
        let write_entries = |w: &mut Writer, entries: &[MemEntry]| {
            for e in entries {
                w.u64(e.addr).u32(e.length).u32(0);
            }
        };
        let write_cursor = |w: &mut Writer, c: &CursorCmd| {
            w.u32(c.scanout_id).u32(c.x).u32(c.y).u32(0).u32(c.resource_id).u32(c.hot_x).u32(c.hot_y).u32(0);
        };
        let write_transfer = |w: &mut Writer, t: &Transfer3d| {
            let b = t.box_;
            w.u32(b.x).u32(b.y).u32(b.z).u32(b.w).u32(b.h).u32(b.d);
            w.u64(t.offset).u32(t.resource_id).u32(t.level).u32(t.stride).u32(t.layer_stride);
        };
        match &self.cmd {
            Command::GetDisplayInfo | Command::CtxDestroy => {}
            Command::ResourceCreate2d { resource_id, format, width, height } => {
                w.u32(*resource_id).u32(*format).u32(*width).u32(*height);
            }
            Command::ResourceUnref { resource_id }
            | Command::ResourceDetachBacking { resource_id }
            | Command::ResourceAssignUuid { resource_id }
            | Command::CtxAttachResource { resource_id }
            | Command::CtxDetachResource { resource_id }
            | Command::ResourceUnmapBlob { resource_id } => {
                w.u32(*resource_id).u32(0);
            }
            Command::SetScanout { rect, scanout_id, resource_id } => {
                w.rect(*rect).u32(*scanout_id).u32(*resource_id);
            }
            Command::ResourceFlush { rect, resource_id } => {
                w.rect(*rect).u32(*resource_id).u32(0);
            }
            Command::TransferToHost2d { rect, offset, resource_id } => {
                w.rect(*rect).u64(*offset).u32(*resource_id).u32(0);
            }
            Command::ResourceAttachBacking { resource_id, entries } => {
                w.u32(*resource_id).u32(entries.len() as u32);
                write_entries(&mut w, entries);
            }
            Command::GetCapsetInfo { capset_index } => {
                w.u32(*capset_index).u32(0);
            }
            Command::GetCapset { capset_id, capset_version } => {
                w.u32(*capset_id).u32(*capset_version);
            }
            Command::GetEdid { scanout } => {
                w.u32(*scanout).u32(0);
            }
            Command::ResourceCreateBlob { resource_id, blob_mem, blob_flags, blob_id, size, entries } => {
                w.u32(*resource_id).u32(*blob_mem).u32(*blob_flags).u32(entries.len() as u32).u64(*blob_id).u64(*size);
                write_entries(&mut w, entries);
            }
            Command::SetScanoutBlob { rect, scanout_id, resource_id, width, height, format, strides, offsets } => {
                w.rect(*rect).u32(*scanout_id).u32(*resource_id).u32(*width).u32(*height).u32(*format).u32(0);
                for s in strides {
                    w.u32(*s);
                }
                for o in offsets {
                    w.u32(*o);
                }
            }
            Command::CtxCreate { context_init, debug_name } => {
                let n = debug_name.len().min(64);
                let mut name = [0u8; 64];
                name[..n].copy_from_slice(&debug_name[..n]);
                w.u32(n as u32).u32(*context_init).bytes(&name);
            }
            Command::ResourceCreate3d(c) => {
                w.u32(c.resource_id).u32(c.target).u32(c.format).u32(c.bind).u32(c.width).u32(c.height);
                w.u32(c.depth).u32(c.array_size).u32(c.last_level).u32(c.nr_samples).u32(c.flags).u32(0);
            }
            Command::TransferToHost3d(t) | Command::TransferFromHost3d(t) => write_transfer(&mut w, t),
            Command::Submit3d { data } => {
                w.u32(data.len() as u32).u32(0).bytes(data);
            }
            Command::ResourceMapBlob { resource_id, offset } => {
                w.u32(*resource_id).u32(0).u64(*offset);
            }
            Command::UpdateCursor(c) | Command::MoveCursor(c) => write_cursor(&mut w, c),
        }
        w.0
    }
}

/// One scanout in a display info response (`virtio_gpu_display_one`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DisplayOne {
    pub rect: Rect,
    pub enabled: bool,
    pub flags: u32,
}

/// A response's body, by type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Response {
    OkNoData,
    /// Up to MAX_SCANOUTS; the rest are written as disabled.
    OkDisplayInfo(Vec<DisplayOne>),
    OkCapsetInfo { capset_id: u32, max_version: u32, max_size: u32 },
    OkCapset(Vec<u8>),
    /// The EDID blob (at most 1024 bytes).
    OkEdid(Vec<u8>),
    OkResourceUuid([u8; 16]),
    OkMapInfo { map_info: u32 },
    Err(u32),
}

impl Response {
    pub fn type_(&self) -> u32 {
        use cmd::*;
        match self {
            Response::OkNoData => RESP_OK_NODATA,
            Response::OkDisplayInfo(_) => RESP_OK_DISPLAY_INFO,
            Response::OkCapsetInfo { .. } => RESP_OK_CAPSET_INFO,
            Response::OkCapset(_) => RESP_OK_CAPSET,
            Response::OkEdid(_) => RESP_OK_EDID,
            Response::OkResourceUuid(_) => RESP_OK_RESOURCE_UUID,
            Response::OkMapInfo { .. } => RESP_OK_MAP_INFO,
            Response::Err(code) => *code,
        }
    }

    /// The wire form, answering a request with header `request`.
    pub fn serialize(&self, request: &CtrlHdr) -> Vec<u8> {
        let mut w = Writer::default();
        request.response(self.type_()).write(&mut w);
        match self {
            Response::OkNoData | Response::Err(_) => {}
            Response::OkDisplayInfo(modes) => {
                for i in 0..MAX_SCANOUTS {
                    let m = modes.get(i).copied().unwrap_or_default();
                    w.rect(m.rect).u32(m.enabled as u32).u32(m.flags);
                }
            }
            Response::OkCapsetInfo { capset_id, max_version, max_size } => {
                w.u32(*capset_id).u32(*max_version).u32(*max_size).u32(0);
            }
            Response::OkCapset(data) => {
                w.bytes(data);
            }
            Response::OkEdid(edid) => {
                let n = edid.len().min(1024);
                let mut blob = [0u8; 1024];
                blob[..n].copy_from_slice(&edid[..n]);
                w.u32(n as u32).u32(0).bytes(&blob);
            }
            Response::OkResourceUuid(uuid) => {
                w.bytes(uuid);
            }
            Response::OkMapInfo { map_info } => {
                w.u32(*map_info).u32(0);
            }
        }
        w.0
    }

    /// Parse a response (what the guest driver does): tests and spikes.
    pub fn parse(buf: &[u8]) -> Result<(CtrlHdr, Self), ParseError> {
        use cmd::*;
        let mut r = Reader::new(buf);
        let hdr = CtrlHdr::read(&mut r)?;
        let body = match hdr.type_ {
            RESP_OK_NODATA => Response::OkNoData,
            RESP_OK_DISPLAY_INFO => {
                let mut modes = Vec::with_capacity(MAX_SCANOUTS);
                for _ in 0..MAX_SCANOUTS {
                    let rect = Rect::read(&mut r)?;
                    let enabled = r.u32()? != 0;
                    let flags = r.u32()?;
                    modes.push(DisplayOne { rect, enabled, flags });
                }
                Response::OkDisplayInfo(modes)
            }
            RESP_OK_CAPSET_INFO => {
                let v = Response::OkCapsetInfo { capset_id: r.u32()?, max_version: r.u32()?, max_size: r.u32()? };
                r.u32()?;
                v
            }
            RESP_OK_CAPSET => Response::OkCapset(r.rest().to_vec()),
            RESP_OK_EDID => {
                let size = r.u32()? as usize;
                r.u32()?;
                let blob = r.bytes(1024)?;
                Response::OkEdid(blob[..size.min(1024)].to_vec())
            }
            RESP_OK_RESOURCE_UUID => Response::OkResourceUuid(r.bytes(16)?.try_into().unwrap()),
            RESP_OK_MAP_INFO => {
                let map_info = r.u32()?;
                r.u32()?;
                Response::OkMapInfo { map_info }
            }
            code @ RESP_ERR_UNSPEC..=RESP_ERR_INVALID_PARAMETER => Response::Err(code),
            other => return Err(ParseError::Unknown(other)),
        };
        Ok((hdr, body))
    }
}

/// `virtio_gpu_config`, the device-specific configuration space (16 bytes).
///
/// The guest only reads `events_read`, `num_scanouts` and `num_capsets`,
/// and writes `events_clear` to acknowledge events. See device.rs for why
/// berth never raises `events_read`: the custom Virtio API gives the device
/// no callback for that write.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DeviceConfig {
    pub events_read: u32,
    pub events_clear: u32,
    pub num_scanouts: u32,
    pub num_capsets: u32,
}

impl DeviceConfig {
    pub const SIZE: usize = 16;

    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&self.events_read.to_le_bytes());
        b[4..8].copy_from_slice(&self.events_clear.to_le_bytes());
        b[8..12].copy_from_slice(&self.num_scanouts.to_le_bytes());
        b[12..16].copy_from_slice(&self.num_capsets.to_le_bytes());
        b
    }

    pub fn parse(b: &[u8]) -> Result<Self, ParseError> {
        let mut r = Reader::new(b);
        Ok(Self { events_read: r.u32()?, events_clear: r.u32()?, num_scanouts: r.u32()?, num_capsets: r.u32()? })
    }
}

/// A 128-byte EDID 1.4 base block for a `width` x `height` display at 60 Hz:
/// what GET_EDID answers, so the guest's mode list starts with the size we
/// want (Linux uses it in place of display info's mode when EDID is offered).
pub fn edid(width: u32, height: u32) -> [u8; 128] {
    // A detailed timing descriptor can describe up to 4095x4095.
    let (w, h) = (width.clamp(1, 4095), height.clamp(1, 4095));
    let mut e = [0u8; 128];
    e[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    // Manufacturer "BRT" in 5-bit letters, big-endian.
    let id = ((b'B' - b'@') as u16) << 10 | ((b'R' - b'@') as u16) << 5 | (b'T' - b'@') as u16;
    e[8..10].copy_from_slice(&id.to_be_bytes());
    e[10..12].copy_from_slice(&1u16.to_le_bytes()); // product code
    e[16] = 1; // week
    e[17] = (2026 - 1990) as u8; // year
    e[18] = 1; // EDID 1.4
    e[19] = 4;
    e[20] = 0xa5; // digital, 8 bits per colour, DisplayPort
    // Physical size in cm, as for a ~110 dpi panel: guests then pick scale 1.
    e[21] = (w as u64 * 254 / 1100 / 10).clamp(1, 255) as u8;
    e[22] = (h as u64 * 254 / 1100 / 10).clamp(1, 255) as u8;
    e[23] = 120; // gamma 2.2
    e[24] = 0x06; // sRGB default, preferred timing is native
    // Chromaticity: sRGB primaries.
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    // No established or standard timings (0x01 0x01 marks a standard slot unused).
    for slot in e[38..54].chunks_mut(2) {
        slot.copy_from_slice(&[0x01, 0x01]);
    }
    // Detailed timing: reduced blanking (CVT-RB style) at 60 Hz.
    let (hblank, vblank) = (160u32, 35u32);
    let (hfront, hsync, vfront, vsync) = (48u32, 32u32, 3u32, 5u32);
    let clock_10khz = ((w + hblank) as u64 * (h + vblank) as u64 * 60 / 10_000).min(0xffff) as u16;
    let d = &mut e[54..72];
    d[0..2].copy_from_slice(&clock_10khz.to_le_bytes());
    d[2] = (w & 0xff) as u8;
    d[3] = (hblank & 0xff) as u8;
    d[4] = (((w >> 8) & 0xf) << 4 | ((hblank >> 8) & 0xf)) as u8;
    d[5] = (h & 0xff) as u8;
    d[6] = (vblank & 0xff) as u8;
    d[7] = (((h >> 8) & 0xf) << 4 | ((vblank >> 8) & 0xf)) as u8;
    d[8] = (hfront & 0xff) as u8;
    d[9] = (hsync & 0xff) as u8;
    d[10] = (((vfront & 0xf) << 4) | (vsync & 0xf)) as u8;
    d[11] = (((hfront >> 8) & 3) << 6 | ((hsync >> 8) & 3) << 4 | ((vfront >> 4) & 3) << 2 | ((vsync >> 4) & 3)) as u8;
    let (mm_w, mm_h) = (e[21] as u32 * 10, e[22] as u32 * 10);
    let d = &mut e[54..72];
    d[12] = (mm_w & 0xff) as u8;
    d[13] = (mm_h & 0xff) as u8;
    d[14] = (((mm_w >> 8) & 0xf) << 4 | ((mm_h >> 8) & 0xf)) as u8;
    d[17] = 0x1e; // digital separate sync, +hsync -vsync (CVT-RB)
    // Display name.
    let name = &mut e[72..90];
    name[0..5].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
    let text = b"berth\n           ";
    name[5..18].copy_from_slice(&text[..13]);
    // Range limits: 50-75 Hz vertical, 30-255 kHz horizontal, max clock.
    let range = &mut e[90..108];
    range[0..5].copy_from_slice(&[0, 0, 0, 0xfd, 0]);
    range[5..10].copy_from_slice(&[50, 75, 30, 255, ((clock_10khz as u32 / 1000) + 1).min(255) as u8]);
    range[10] = 0x01; // no timing formula
    range[11..18].copy_from_slice(&[0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20]);
    // Last descriptor: dummy.
    e[108..126].copy_from_slice(&[0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    e[126] = 0; // no extension blocks
    let sum = e[..127].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    e[127] = 0u8.wrapping_sub(sum);
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(flags: u32, fence_id: u64) -> CtrlHdr {
        CtrlHdr { type_: 0, flags, fence_id, ctx_id: 7, ring_idx: 2 }
    }

    /// Every command survives serialize -> parse unchanged.
    #[test]
    fn every_command_round_trips() {
        let entries = vec![MemEntry { addr: 0x8000_0000, length: 4096 }, MemEntry { addr: 0x9000_0000, length: 8192 }];
        let cur = CursorCmd { scanout_id: 0, x: 10, y: 20, resource_id: 5, hot_x: 1, hot_y: 2 };
        let t3 = Transfer3d {
            box_: Box3d { x: 1, y: 2, z: 3, w: 4, h: 5, d: 6 },
            offset: 99,
            resource_id: 3,
            level: 1,
            stride: 256,
            layer_stride: 0,
        };
        let commands = vec![
            Command::GetDisplayInfo,
            Command::ResourceCreate2d { resource_id: 1, format: 2, width: 640, height: 480 },
            Command::ResourceUnref { resource_id: 1 },
            Command::SetScanout { rect: Rect::new(0, 0, 640, 480), scanout_id: 0, resource_id: 1 },
            Command::ResourceFlush { rect: Rect::new(1, 2, 3, 4), resource_id: 1 },
            Command::TransferToHost2d { rect: Rect::new(1, 2, 3, 4), offset: 0x1234, resource_id: 1 },
            Command::ResourceAttachBacking { resource_id: 1, entries: entries.clone() },
            Command::ResourceDetachBacking { resource_id: 1 },
            Command::GetCapsetInfo { capset_index: 0 },
            Command::GetCapset { capset_id: capset::VIRGL2, capset_version: 2 },
            Command::GetEdid { scanout: 0 },
            Command::ResourceAssignUuid { resource_id: 1 },
            Command::ResourceCreateBlob {
                resource_id: 2,
                blob_mem: blob_mem::GUEST,
                blob_flags: blob_flags::USE_SHAREABLE,
                blob_id: 0,
                size: 12288,
                entries,
            },
            Command::SetScanoutBlob {
                rect: Rect::new(0, 0, 64, 64),
                scanout_id: 0,
                resource_id: 2,
                width: 64,
                height: 64,
                format: 2,
                strides: [256, 0, 0, 0],
                offsets: [0, 0, 0, 0],
            },
            Command::CtxCreate { context_init: capset::VENUS, debug_name: b"weston".to_vec() },
            Command::CtxDestroy,
            Command::CtxAttachResource { resource_id: 9 },
            Command::CtxDetachResource { resource_id: 9 },
            Command::ResourceCreate3d(Create3d { resource_id: 4, target: 2, format: 1, width: 8, height: 8, depth: 1, ..Default::default() }),
            Command::TransferToHost3d(t3),
            Command::TransferFromHost3d(t3),
            Command::Submit3d { data: vec![1, 2, 3, 4, 5] },
            Command::ResourceMapBlob { resource_id: 2, offset: 0x10000 },
            Command::ResourceUnmapBlob { resource_id: 2 },
            Command::UpdateCursor(cur),
            Command::MoveCursor(cur),
        ];
        for cmd in commands {
            let req = Request { hdr: hdr(FLAG_FENCE, 42), cmd: cmd.clone() };
            let bytes = req.serialize();
            let parsed = Request::parse(&bytes).unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
            assert_eq!(parsed.cmd, cmd);
            assert_eq!(parsed.hdr.fence_id, 42);
            assert_eq!(parsed.hdr.ctx_id, 7);
            // And one byte short is an error, not a panic.
            assert!(matches!(Request::parse(&bytes[..bytes.len() - 1]), Err(ParseError::Short { .. })), "{cmd:?}");
        }
    }

    /// Sizes from the spec's structs (Linux's virtio_gpu.h).
    #[test]
    fn struct_sizes_match_the_spec() {
        let size = |cmd: Command| Request { hdr: CtrlHdr::default(), cmd }.serialize().len();
        assert_eq!(size(Command::GetDisplayInfo), 24);
        assert_eq!(size(Command::ResourceCreate2d { resource_id: 0, format: 0, width: 0, height: 0 }), 40);
        assert_eq!(size(Command::ResourceUnref { resource_id: 0 }), 32);
        assert_eq!(size(Command::SetScanout { rect: Rect::default(), scanout_id: 0, resource_id: 0 }), 48);
        assert_eq!(size(Command::ResourceFlush { rect: Rect::default(), resource_id: 0 }), 48);
        assert_eq!(size(Command::TransferToHost2d { rect: Rect::default(), offset: 0, resource_id: 0 }), 56);
        assert_eq!(size(Command::ResourceAttachBacking { resource_id: 0, entries: vec![] }), 32);
        assert_eq!(size(Command::GetEdid { scanout: 0 }), 32);
        assert_eq!(
            size(Command::ResourceCreateBlob { resource_id: 0, blob_mem: 0, blob_flags: 0, blob_id: 0, size: 0, entries: vec![] }),
            56
        );
        assert_eq!(
            size(Command::SetScanoutBlob {
                rect: Rect::default(),
                scanout_id: 0,
                resource_id: 0,
                width: 0,
                height: 0,
                format: 0,
                strides: [0; 4],
                offsets: [0; 4]
            }),
            96
        );
        assert_eq!(size(Command::CtxCreate { context_init: 0, debug_name: vec![] }), 96);
        assert_eq!(size(Command::ResourceCreate3d(Create3d::default())), 72);
        assert_eq!(size(Command::TransferToHost3d(Transfer3d::default())), 72);
        assert_eq!(size(Command::Submit3d { data: vec![] }), 32);
        assert_eq!(size(Command::ResourceMapBlob { resource_id: 0, offset: 0 }), 40);
        assert_eq!(size(Command::UpdateCursor(CursorCmd::default())), 56);
        let r = |resp: Response| resp.serialize(&CtrlHdr::default()).len();
        assert_eq!(r(Response::OkNoData), 24);
        assert_eq!(r(Response::OkDisplayInfo(vec![])), 24 + 16 * 24);
        assert_eq!(r(Response::OkCapsetInfo { capset_id: 0, max_version: 0, max_size: 0 }), 40);
        assert_eq!(r(Response::OkEdid(vec![0; 128])), 24 + 8 + 1024);
        assert_eq!(r(Response::OkResourceUuid([0; 16])), 40);
        assert_eq!(r(Response::OkMapInfo { map_info: 0 }), 32);
    }

    #[test]
    fn every_response_round_trips() {
        let request = hdr(FLAG_FENCE | FLAG_INFO_RING_IDX, 9);
        let responses = vec![
            Response::OkNoData,
            Response::OkDisplayInfo(
                (0..MAX_SCANOUTS)
                    .map(|i| DisplayOne { rect: Rect::new(0, 0, 100 + i as u32, 50), enabled: i == 0, flags: 0 })
                    .collect(),
            ),
            Response::OkCapsetInfo { capset_id: capset::VIRGL2, max_version: 2, max_size: 1000 },
            Response::OkCapset(vec![1, 2, 3]),
            Response::OkEdid(edid(1920, 1080).to_vec()),
            Response::OkResourceUuid([7; 16]),
            Response::OkMapInfo { map_info: map_cache::CACHED },
            Response::Err(cmd::RESP_ERR_INVALID_RESOURCE_ID),
        ];
        for resp in responses {
            let bytes = resp.serialize(&request);
            let (h, parsed) = Response::parse(&bytes).unwrap();
            assert_eq!(parsed, resp);
            assert_eq!(h.type_, resp.type_());
            assert_eq!((h.flags, h.fence_id, h.ctx_id, h.ring_idx), (FLAG_FENCE | FLAG_INFO_RING_IDX, 9, 7, 2));
        }
    }

    #[test]
    fn unfenced_requests_get_unfenced_responses() {
        let h = hdr(FLAG_INFO_RING_IDX, 55).response(cmd::RESP_OK_NODATA);
        assert_eq!((h.flags, h.fence_id, h.ring_idx), (0, 0, 0));
        let h = hdr(FLAG_FENCE, 55).response(cmd::RESP_OK_NODATA);
        assert_eq!((h.flags, h.fence_id, h.ring_idx), (FLAG_FENCE, 55, 0));
    }

    #[test]
    fn hostile_requests_are_errors() {
        assert_eq!(Request::parse(&[0; 10]), Err(ParseError::Short { need: 4 + 4 + 8, have: 10 }));
        let mut h = Vec::new();
        h.extend_from_slice(&0x0999u32.to_le_bytes());
        h.extend_from_slice(&[0; 20]);
        assert_eq!(Request::parse(&h), Err(ParseError::Unknown(0x0999)));
        // An attach claiming a huge entry count, without the entries.
        let mut b = Request { hdr: CtrlHdr::default(), cmd: Command::ResourceAttachBacking { resource_id: 1, entries: vec![] } }
            .serialize();
        b[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Request::parse(&b), Err(ParseError::TooManyEntries(u32::MAX)));
        b[28..32].copy_from_slice(&1000u32.to_le_bytes());
        assert!(matches!(Request::parse(&b), Err(ParseError::Short { .. })));
        // A submit claiming more data than it carries.
        let mut s = Request { hdr: CtrlHdr::default(), cmd: Command::Submit3d { data: vec![0; 8] } }.serialize();
        s[24..28].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(matches!(Request::parse(&s), Err(ParseError::Short { .. })));
        // Debug names longer than the field are cut, not overread.
        let mut c = Request { hdr: CtrlHdr::default(), cmd: Command::CtxCreate { context_init: 0, debug_name: b"x".to_vec() } }
            .serialize();
        c[24..28].copy_from_slice(&500u32.to_le_bytes());
        let Command::CtxCreate { debug_name, .. } = Request::parse(&c).unwrap().cmd else { panic!() };
        assert_eq!(debug_name.len(), 64);
    }

    #[test]
    fn config_space_layout() {
        let c = DeviceConfig { events_read: 0, events_clear: 0, num_scanouts: 1, num_capsets: 2 };
        let b = c.to_bytes();
        assert_eq!(&b[8..12], &1u32.to_le_bytes());
        assert_eq!(&b[12..16], &2u32.to_le_bytes());
        assert_eq!(DeviceConfig::parse(&b).unwrap(), c);
    }

    #[test]
    fn rect_math_does_not_overflow() {
        assert!(Rect::new(0, 0, 640, 480).fits(640, 480));
        assert!(!Rect::new(1, 0, 640, 480).fits(640, 480));
        assert!(!Rect::new(u32::MAX, 0, 2, 1).fits(640, 480));
        assert!(!Rect::new(0, u32::MAX, 1, u32::MAX).fits(u32::MAX, u32::MAX));
        assert_eq!(Rect::new(0, 0, 10, 10).intersect(&Rect::new(5, 5, 10, 10)), Some(Rect::new(5, 5, 5, 5)));
        assert_eq!(Rect::new(0, 0, 10, 10).intersect(&Rect::new(10, 0, 5, 5)), None);
        assert_eq!(Rect::new(u32::MAX - 1, 0, u32::MAX, 1).intersect(&Rect::new(0, 0, u32::MAX, 1)), Some(Rect::new(u32::MAX - 1, 0, 1, 1)));
    }

    #[test]
    fn formats() {
        for v in [1, 2, 3, 4, 67, 68, 121, 134] {
            assert_eq!(Format::from_u32(v).unwrap() as u32, v);
        }
        assert_eq!(Format::from_u32(5), None);
    }

    #[test]
    fn edid_is_valid_and_describes_the_mode() {
        for (w, h) in [(1920, 1080), (2560, 1600), (3840, 2160), (640, 480)] {
            let e = edid(w, h);
            assert_eq!(&e[..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
            assert_eq!(e.iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0, "checksum");
            let d = &e[54..72];
            let hactive = d[2] as u32 | ((d[4] as u32 >> 4) << 8);
            let vactive = d[5] as u32 | ((d[7] as u32 >> 4) << 8);
            assert_eq!((hactive, vactive), (w, h));
            let clock = u16::from_le_bytes([d[0], d[1]]) as u32 * 10_000;
            let refresh = clock as f64 / ((w + 160) as f64 * (h + 35) as f64);
            assert!((59.0..61.0).contains(&refresh), "{refresh}");
            assert_eq!(&e[77..83], b"berth\n");
        }
    }
}
