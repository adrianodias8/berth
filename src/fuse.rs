//! A passthrough FUSE server for virtio-fs: Linux FUSE requests in, replies
//! out, backed by a directory on the Mac. Used by the experimental berth
//! virtio-fs device (virtiofs.rs) on macOS 27's custom Virtio API, which lets
//! us choose the caching policy that Apple's built-in virtiofs doesn't expose.
//!
//! Protocol structures follow Linux include/uapi/linux/fuse.h. macOS and
//! Linux differ in errno and open(2) flag values, so both are translated.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FUSE_ROOT_ID: u64 = 1;
const KERNEL_MINOR: u32 = 31;

mod op {
    pub const LOOKUP: u32 = 1;
    pub const FORGET: u32 = 2;
    pub const GETATTR: u32 = 3;
    pub const SETATTR: u32 = 4;
    pub const READLINK: u32 = 5;
    pub const SYMLINK: u32 = 6;
    pub const MKDIR: u32 = 9;
    pub const UNLINK: u32 = 10;
    pub const RMDIR: u32 = 11;
    pub const RENAME: u32 = 12;
    pub const LINK: u32 = 13;
    pub const OPEN: u32 = 14;
    pub const READ: u32 = 15;
    pub const WRITE: u32 = 16;
    pub const STATFS: u32 = 17;
    pub const RELEASE: u32 = 18;
    pub const FSYNC: u32 = 20;
    pub const FLUSH: u32 = 25;
    pub const INIT: u32 = 26;
    pub const OPENDIR: u32 = 27;
    pub const READDIR: u32 = 28;
    pub const RELEASEDIR: u32 = 29;
    pub const FSYNCDIR: u32 = 30;
    pub const ACCESS: u32 = 34;
    pub const CREATE: u32 = 35;
    pub const INTERRUPT: u32 = 36;
    pub const DESTROY: u32 = 38;
    pub const BATCH_FORGET: u32 = 42;
    pub const READDIRPLUS: u32 = 44;
    pub const RENAME2: u32 = 45;
}

// FUSE_INIT flags we may enable.
const FUSE_ASYNC_READ: u32 = 1 << 0;
const FUSE_BIG_WRITES: u32 = 1 << 5;
const FUSE_AUTO_INVAL_DATA: u32 = 1 << 12;
const FUSE_DO_READDIRPLUS: u32 = 1 << 13;
const FUSE_READDIRPLUS_AUTO: u32 = 1 << 14;
const FUSE_NO_OPEN_SUPPORT: u32 = 1 << 17;
const FUSE_WRITEBACK_CACHE: u32 = 1 << 16;
const FUSE_PARALLEL_DIROPS: u32 = 1 << 18;
const FUSE_MAX_PAGES: u32 = 1 << 22;
const FUSE_CACHE_SYMLINKS: u32 = 1 << 23;

// SETATTR valid bits.
const FATTR_MODE: u32 = 1 << 0;
const FATTR_UID: u32 = 1 << 1;
const FATTR_GID: u32 = 1 << 2;
const FATTR_SIZE: u32 = 1 << 3;
const FATTR_ATIME: u32 = 1 << 4;
const FATTR_MTIME: u32 = 1 << 5;
const FATTR_ATIME_NOW: u32 = 1 << 7;
const FATTR_MTIME_NOW: u32 = 1 << 8;

const FOPEN_KEEP_CACHE: u32 = 1 << 1;
const MAX_WRITE: u32 = 1 << 20;
/// Files kept open for no-open reads and writes; past this the oldest are
/// closed (the kernel never says when it's done with them).
const MAX_CACHED_FILES: usize = 4096;
/// Async requests (readahead, writeback) the kernel may have in flight.
const MAX_BACKGROUND: u16 = 64;

/// Linux errno values (FUSE replies carry the negated Linux number).
mod lerr {
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EBADF: i32 = 9;
    pub const EINVAL: i32 = 22;
    pub const ENOSYS: i32 = 38;
    pub const ESTALE: i32 = 116;
}

/// BERTH_FS_TRACE=1: log every request's opcode and node (for tuning).
fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BERTH_FS_TRACE").is_some())
}

/// macOS errno → Linux errno. 1–34 are the same on both.
fn linux_errno(mac: i32) -> i32 {
    match mac {
        1..=34 => mac,
        libc::EAGAIN => 11,
        libc::ENAMETOOLONG => 36,
        libc::ENOSYS => 38,
        libc::ENOTEMPTY => 39,
        libc::ELOOP => 40,
        libc::ENOATTR => 61,
        libc::EOVERFLOW => 75,
        libc::ENOTSUP => 95,
        libc::ETIMEDOUT => 110,
        libc::EDQUOT => 122,
        _ => lerr::EIO,
    }
}

fn io_errno(e: &std::io::Error) -> i32 {
    e.raw_os_error().map_or(lerr::EIO, linux_errno)
}

/// Linux open(2) flags → macOS. The guest is arm64, whose O_DIRECTORY,
/// O_NOFOLLOW, O_DIRECT and O_LARGEFILE differ from the generic values
/// (arch/arm64/include/uapi/asm/fcntl.h); O_DIRECT and O_LARGEFILE have no
/// macOS meaning and are dropped.
fn mac_open_flags(linux: u32) -> i32 {
    const O_DSYNC: u32 = 0o10000;
    const O_SYNC: u32 = 0o4000000 | O_DSYNC;
    let mut flags = (linux & 3) as i32; // O_RDONLY/O_WRONLY/O_RDWR match
    for (l, m) in [
        (0o100, libc::O_CREAT),
        (0o200, libc::O_EXCL),
        (0o1000, libc::O_TRUNC),
        (0o2000, libc::O_APPEND),
        (0o4000, libc::O_NONBLOCK),
        (0o40000, libc::O_DIRECTORY),
        (0o100000, libc::O_NOFOLLOW),
    ] {
        if linux & l != 0 {
            flags |= m;
        }
    }
    if linux & O_SYNC == O_SYNC {
        flags |= libc::O_SYNC;
    } else if linux & O_DSYNC != 0 {
        flags |= libc::O_DSYNC;
    }
    flags | libc::O_CLOEXEC
}

/// How long the guest may trust names and attributes without asking again.
/// This is the knob Apple's virtiofs doesn't give us.
#[derive(Clone, Copy, Debug)]
pub struct CachePolicy {
    pub entry: Duration,
    pub attr: Duration,
    /// How long the guest may remember that a name doesn't exist. PHP,
    /// Composer and compilers probe many missing paths; each costs a round
    /// trip otherwise. Short, since a file created on the Mac stays
    /// invisible to the guest this long.
    pub negative: Duration,
    pub writeback: bool,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            entry: Duration::from_secs(1),
            attr: Duration::from_secs(1),
            negative: Duration::from_secs(1),
            writeback: false,
        }
    }
}

struct Node {
    path: PathBuf,
    lookups: u64,
    /// With "no open" mode the kernel skips OPEN/RELEASE and sends fh 0;
    /// reads and writes then use this per-node file, opened on first use.
    file: Option<Arc<fs::File>>,
}

struct Dir {
    path: PathBuf,
    entries: Vec<(Vec<u8>, u64, u32)>,
}

#[derive(Clone)]
enum Handle {
    File(Arc<fs::File>),
    Dir(Arc<Dir>),
}

/// Safe to call from several threads at once: the tables are locked only
/// to look up or record paths and handles, never across a syscall, so
/// requests from different guest threads hit the Mac's filesystem in
/// parallel.
pub struct Server {
    root: PathBuf,
    policy: CachePolicy,
    /// nodeid → node. nodeid is the file's inode number (root is 1).
    nodes: Mutex<HashMap<u64, Node>>,
    /// Nodes holding a cached file, oldest first.
    cached: Mutex<std::collections::VecDeque<u64>>,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    /// Requests seen, and nanoseconds spent, per opcode, for tuning.
    opcodes: [AtomicU64; 64],
    opcode_nanos: [AtomicU64; 64],
    /// The kernel supports skipping OPEN/RELEASE (FUSE_NO_OPEN_SUPPORT).
    no_open: AtomicBool,
}

/// Little-endian cursor over a request.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let b = self.buf.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(b)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
    /// A NUL-terminated name.
    fn cstr(&mut self) -> Option<&'a [u8]> {
        let rest = self.buf.get(self.pos..)?;
        let end = rest.iter().position(|&b| b == 0)?;
        self.pos += end + 1;
        Some(&rest[..end])
    }
    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }
}

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
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
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn zeros(&mut self, n: usize) -> &mut Self {
        self.0.resize(self.0.len() + n, 0);
        self
    }
}

type Reply = Result<Vec<u8>, i32>;

impl Server {
    pub fn new(root: PathBuf, policy: CachePolicy) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(FUSE_ROOT_ID, Node { path: root.clone(), lookups: 1, file: None });
        Self {
            root,
            policy,
            nodes: Mutex::new(nodes),
            cached: Mutex::new(std::collections::VecDeque::new()),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            opcodes: std::array::from_fn(|_| AtomicU64::new(0)),
            opcode_nanos: std::array::from_fn(|_| AtomicU64::new(0)),
            no_open: AtomicBool::new(false),
        }
    }

    /// (opcode, requests, average µs) for every opcode seen, most frequent first.
    pub fn opcode_counts(&self) -> Vec<(u32, u64, f64)> {
        let mut counts: Vec<_> = (0..64usize)
            .map(|op| (op as u32, self.opcodes[op].load(Ordering::Relaxed), self.opcode_nanos[op].load(Ordering::Relaxed)))
            .filter(|(_, n, _)| *n > 0)
            .map(|(op, n, ns)| (op, n, ns as f64 / n as f64 / 1000.0))
            .collect();
        counts.sort_unstable_by_key(|&(_, n, _)| std::cmp::Reverse(n));
        counts
    }

    /// Handle one request. Returns the reply bytes, or None when the opcode
    /// expects no reply (FORGET).
    pub fn handle(&self, request: &[u8]) -> Option<Vec<u8>> {
        let started = std::time::Instant::now();
        let reply = self.serve(request);
        if let Some(op) = request.get(4..8).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            && op < 64
        {
            self.opcode_nanos[op].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        reply
    }

    fn serve(&self, request: &[u8]) -> Option<Vec<u8>> {
        let mut r = Reader::new(request);
        let (_len, opcode, unique, nodeid) = (r.u32()?, r.u32()?, r.u64()?, r.u64()?);
        if trace() {
            let name = request.get(40..).and_then(|b| b.split(|&c| c == 0).next()).filter(|_| matches!(opcode, op::LOOKUP | op::CREATE | op::MKDIR | op::UNLINK));
            let valid = (opcode == op::SETATTR).then(|| request.get(40..44).map(|b| u32::from_le_bytes(b.try_into().unwrap()))).flatten();
            let size = (opcode == op::SETATTR).then(|| request.get(56..64).map(|b| u64::from_le_bytes(b.try_into().unwrap()))).flatten();
            eprintln!("fuse: op {opcode} node {nodeid}{}{}", valid.map(|v| format!(" valid {v:#x} size {}", size.unwrap_or(0))).unwrap_or_default(), name.map(|n| format!(" {:?}", String::from_utf8_lossy(&n[..n.len().min(40)]))).unwrap_or_default());
        }
        if let Some(n) = self.opcodes.get(opcode as usize) {
            n.fetch_add(1, Ordering::Relaxed);
        }
        let (_uid, _gid, _pid, _extlen, _pad) = (r.u32()?, r.u32()?, r.u32()?, r.u16()?, r.u16()?);
        let result = match opcode {
            op::FORGET => {
                let n = r.u64().unwrap_or(0);
                self.forget(nodeid, n);
                return None;
            }
            op::BATCH_FORGET => {
                let count = r.u32().unwrap_or(0);
                let _ = r.u32();
                for _ in 0..count {
                    if let (Some(id), Some(n)) = (r.u64(), r.u64()) {
                        self.forget(id, n);
                    }
                }
                return None;
            }
            op::INTERRUPT => return None,
            op::INIT => self.init(&mut r),
            op::DESTROY => Ok(Vec::new()),
            op::LOOKUP => self.lookup(nodeid, r.cstr().unwrap_or_default()),
            op::GETATTR => self.getattr(nodeid),
            op::SETATTR => self.setattr(nodeid, &mut r),
            op::READLINK => self.readlink(nodeid),
            op::SYMLINK => {
                let (name, target) = (r.cstr().unwrap_or_default(), r.cstr().unwrap_or_default());
                self.symlink(nodeid, name, target)
            }
            op::MKDIR => {
                let mode = r.u32().unwrap_or(0o755);
                let _umask = r.u32();
                self.mkdir(nodeid, r.cstr().unwrap_or_default(), mode)
            }
            op::UNLINK => self.remove(nodeid, r.cstr().unwrap_or_default(), false),
            op::RMDIR => self.remove(nodeid, r.cstr().unwrap_or_default(), true),
            op::RENAME => {
                let newdir = r.u64().unwrap_or(0);
                let (old, new) = (r.cstr().unwrap_or_default(), r.cstr().unwrap_or_default());
                self.rename(nodeid, old, newdir, new)
            }
            op::RENAME2 => {
                let newdir = r.u64().unwrap_or(0);
                let flags = r.u32().unwrap_or(0);
                let _ = r.u32();
                if flags != 0 {
                    Err(lerr::EINVAL)
                } else {
                    let (old, new) = (r.cstr().unwrap_or_default(), r.cstr().unwrap_or_default());
                    self.rename(nodeid, old, newdir, new)
                }
            }
            op::LINK => {
                let old = r.u64().unwrap_or(0);
                self.link(old, nodeid, r.cstr().unwrap_or_default())
            }
            // ENOSYS tells a no-open-capable kernel to stop sending OPEN and
            // RELEASE for regular files.
            op::OPEN if self.no_open.load(Ordering::Relaxed) => Err(lerr::ENOSYS),
            op::OPEN => self.open(nodeid, r.u32().unwrap_or(0)),
            op::CREATE => {
                let (flags, mode) = (r.u32().unwrap_or(0), r.u32().unwrap_or(0o644));
                let (_umask, _open_flags) = (r.u32(), r.u32());
                self.create(nodeid, r.cstr().unwrap_or_default(), flags, mode)
            }
            op::READ => {
                let (fh, offset, size) = (r.u64().unwrap_or(0), r.u64().unwrap_or(0), r.u32().unwrap_or(0));
                self.read(nodeid, fh, offset, size)
            }
            op::WRITE => {
                let (fh, offset, size) = (r.u64().unwrap_or(0), r.u64().unwrap_or(0), r.u32().unwrap_or(0));
                let _ = (r.u32(), r.u64(), r.u32(), r.u32());
                let data = r.rest();
                self.write(nodeid, fh, offset, &data[..(size as usize).min(data.len())])
            }
            op::RELEASE | op::RELEASEDIR => {
                let fh = r.u64().unwrap_or(0);
                // Dropped outside the lock: closing a file is a syscall.
                let handle = self.handles.lock().unwrap().remove(&fh);
                drop(handle);
                Ok(Vec::new())
            }
            // Nothing to flush (writes go straight to the file); ENOSYS makes
            // the kernel stop sending FLUSH on every close.
            op::FLUSH => Err(lerr::ENOSYS),
            op::FSYNC => self.fsync(nodeid, r.u64().unwrap_or(0)),
            op::FSYNCDIR => Ok(Vec::new()),
            op::STATFS => self.statfs(),
            op::OPENDIR => self.opendir(nodeid),
            op::READDIR => {
                let (fh, offset, size) = (r.u64().unwrap_or(0), r.u64().unwrap_or(0), r.u32().unwrap_or(0));
                self.readdir(fh, offset, size, false)
            }
            op::READDIRPLUS => {
                let (fh, offset, size) = (r.u64().unwrap_or(0), r.u64().unwrap_or(0), r.u32().unwrap_or(0));
                self.readdir(fh, offset, size, true)
            }
            op::ACCESS => self.path(nodeid).map(|_| Vec::new()),
            _ => Err(lerr::ENOSYS),
        };
        // The guest caches names and attributes, so it may refer to a file
        // or directory that was since removed on the Mac. ESTALE (rather
        // than ENOENT) makes its kernel look the path up again, bypassing
        // the cache, and retry: `rm -rf vendor` on the Mac followed by
        // `composer install` in a container then just works.
        let result = match result {
            Err(lerr::ENOENT) if nodeid != 0 && self.vanished(nodeid) => Err(lerr::ESTALE),
            result => result,
        };
        let (error, body) = match result {
            Ok(body) => (0, body),
            Err(errno) => (-errno, Vec::new()),
        };
        let mut out = Out::default();
        out.u32((16 + body.len()) as u32).i32(error).u64(unique);
        out.0.extend_from_slice(&body);
        Some(out.0)
    }

    fn init(&self, r: &mut Reader) -> Reply {
        let (major, minor, max_readahead, kernel_flags) = (
            r.u32().ok_or(lerr::EINVAL)?,
            r.u32().ok_or(lerr::EINVAL)?,
            r.u32().ok_or(lerr::EINVAL)?,
            r.u32().ok_or(lerr::EINVAL)?,
        );
        if major != 7 {
            return Err(lerr::EINVAL);
        }
        self.no_open.store(kernel_flags & FUSE_NO_OPEN_SUPPORT != 0, Ordering::Relaxed);
        // Not FUSE_ATOMIC_O_TRUNC: it moves O_TRUNC into OPEN, which a
        // no-open kernel never sends, so `open(O_TRUNC)` wouldn't truncate
        // (a rewritten shorter file kept its old tail). Without it the
        // kernel truncates with SETATTR first.
        let mut want = FUSE_ASYNC_READ
            | FUSE_BIG_WRITES
            | FUSE_AUTO_INVAL_DATA
            | FUSE_PARALLEL_DIROPS
            | FUSE_MAX_PAGES
            | FUSE_CACHE_SYMLINKS
            | FUSE_DO_READDIRPLUS
            | FUSE_READDIRPLUS_AUTO;
        if self.policy.writeback {
            want |= FUSE_WRITEBACK_CACHE;
        }
        let mut out = Out::default();
        out.u32(7)
            .u32(minor.min(KERNEL_MINOR))
            .u32(max_readahead)
            .u32(kernel_flags & want)
            .u16(MAX_BACKGROUND)
            .u16(MAX_BACKGROUND * 3 / 4) // congestion_threshold
            .u32(MAX_WRITE)
            .u32(1) // time_gran: nanoseconds
            .u16((MAX_WRITE / 4096) as u16) // max_pages
            .u16(0) // map_alignment
            .u32(0) // flags2
            .u32(0) // max_stack_depth
            .zeros(24);
        Ok(out.0)
    }

    fn path(&self, nodeid: u64) -> Result<PathBuf, i32> {
        self.nodes.lock().unwrap().get(&nodeid).map(|n| n.path.clone()).ok_or(lerr::ENOENT)
    }

    /// The node (the file itself, or the directory a name is looked up in)
    /// is gone: unknown to us, or no longer on disk.
    fn vanished(&self, nodeid: u64) -> bool {
        self.path(nodeid).is_err()
            || self.path(nodeid).is_ok_and(|p| fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound))
    }

    /// The node `ino` no longer has the name `path`.
    fn forget_name(&self, ino: u64, path: &Path) {
        if let Some(node) = self.nodes.lock().unwrap().get_mut(&ino)
            && node.path == path
        {
            node.path = PathBuf::new();
        }
    }

    /// The node's path and its cached open file, if any.
    fn path_and_file(&self, nodeid: u64) -> Result<(PathBuf, Option<Arc<fs::File>>), i32> {
        let nodes = self.nodes.lock().unwrap();
        let node = nodes.get(&nodeid).ok_or(lerr::ENOENT)?;
        Ok((node.path.clone(), node.file.clone()))
    }

    fn child(&self, parent: u64, name: &[u8]) -> Result<PathBuf, i32> {
        if name.is_empty() || name.contains(&b'/') {
            return Err(lerr::EINVAL);
        }
        Ok(self.path(parent)?.join(OsStr::from_bytes(name)))
    }

    fn forget(&self, nodeid: u64, n: u64) {
        if nodeid == FUSE_ROOT_ID {
            return;
        }
        let mut nodes = self.nodes.lock().unwrap();
        if let Some(node) = nodes.get_mut(&nodeid) {
            node.lookups = node.lookups.saturating_sub(n);
            if node.lookups == 0 {
                let node = nodes.remove(&nodeid);
                drop(nodes);
                drop(node);
            }
        }
    }

    fn attr(out: &mut Out, nodeid: u64, m: &fs::Metadata) {
        out.u64(nodeid)
            .u64(m.size())
            .u64(m.blocks())
            .u64(m.atime() as u64)
            .u64(m.mtime() as u64)
            .u64(m.ctime() as u64)
            .u32(m.atime_nsec() as u32)
            .u32(m.mtime_nsec() as u32)
            .u32(m.ctime_nsec() as u32)
            .u32(m.mode())
            .u32(m.nlink() as u32)
            .u32(m.uid())
            .u32(m.gid())
            .u32(m.rdev() as u32)
            .u32(m.blksize() as u32)
            .u32(0);
    }

    /// Register (or re-reference) the node at `path` and reply fuse_entry_out.
    fn entry(&self, path: PathBuf) -> Reply {
        let m = fs::symlink_metadata(&path).map_err(|e| io_errno(&e))?;
        self.entry_with(path, &m)
    }

    /// `entry` for a path the guest just created, noted for the watcher.
    fn entry_noted(&self, path: PathBuf) -> Reply {
        let m = fs::symlink_metadata(&path).map_err(|e| io_errno(&e))?;
        crate::watch::guest_wrote(&path, &m);
        self.entry_with(path, &m)
    }

    /// `entry` for a path whose metadata the caller already has.
    fn entry_with(&self, path: PathBuf, m: &fs::Metadata) -> Reply {
        let nodeid = if path == self.root { FUSE_ROOT_ID } else { m.ino() };
        let stale = {
            let mut nodes = self.nodes.lock().unwrap();
            let node = nodes.entry(nodeid).or_insert(Node { path: path.clone(), lookups: 0, file: None });
            let stale = if node.path != path { node.file.take() } else { None };
            node.path = path;
            node.lookups += 1;
            stale
        };
        drop(stale);
        let p = self.policy;
        let mut out = Out::default();
        out.u64(nodeid)
            .u64(0)
            .u64(p.entry.as_secs())
            .u64(p.attr.as_secs())
            .u32(p.entry.subsec_nanos())
            .u32(p.attr.subsec_nanos());
        Self::attr(&mut out, nodeid, m);
        Ok(out.0)
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Reply {
        let path = self.child(parent, name)?;
        match self.entry(path) {
            // A negative entry (node ID 0) the guest may cache.
            Err(lerr::ENOENT) if !self.policy.negative.is_zero() && !self.vanished(parent) => {
                let n = self.policy.negative;
                let mut out = Out::default();
                out.u64(0).u64(0).u64(n.as_secs()).u64(0).u32(n.subsec_nanos()).u32(0).zeros(88);
                Ok(out.0)
            }
            result => result,
        }
    }

    fn attr_reply(&self, nodeid: u64, m: &fs::Metadata) -> Vec<u8> {
        let mut out = Out::default();
        out.u64(self.policy.attr.as_secs()).u32(self.policy.attr.subsec_nanos()).u32(0);
        Self::attr(&mut out, nodeid, m);
        out.0
    }

    fn getattr(&self, nodeid: u64) -> Reply {
        // The open file when there is one: also right after the name was
        // removed or replaced, while the guest still has it open.
        let m = match self.path_and_file(nodeid)? {
            (_, Some(file)) => file.metadata(),
            (path, None) => fs::symlink_metadata(path),
        }
        .map_err(|e| io_errno(&e))?;
        Ok(self.attr_reply(nodeid, &m))
    }

    fn setattr(&self, nodeid: u64, r: &mut Reader) -> Reply {
        let valid = r.u32().ok_or(lerr::EINVAL)?;
        let _pad = r.u32();
        let fh = r.u64().unwrap_or(0);
        let size = r.u64().unwrap_or(0);
        let _lock_owner = r.u64();
        let (atime, mtime, _ctime) = (r.u64().unwrap_or(0), r.u64().unwrap_or(0), r.u64());
        let (atimensec, mtimensec, _ctimensec) = (r.u32().unwrap_or(0), r.u32().unwrap_or(0), r.u32());
        let mode = r.u32().unwrap_or(0);
        let _unused = r.u32();
        let (uid, gid) = (r.u32().unwrap_or(u32::MAX), r.u32().unwrap_or(u32::MAX));
        // Through the open file when there is one (no path lookups), else
        // by path.
        let (path, cached) = self.path_and_file(nodeid)?;
        let file = self.handle_file(fh).or(cached);
        let fd = file.as_ref().map(|f| f.as_raw_fd());
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| lerr::EINVAL)?;
        let stat = || match &file {
            Some(f) => f.metadata(),
            None => fs::symlink_metadata(&path),
        };
        let os_err = || io_errno(&std::io::Error::last_os_error());
        // Attributes known to be current, saving a stat for the reply.
        let mut current = None;

        if valid & FATTR_MODE != 0 {
            // Tools set modes that are already right all the time (Composer
            // chmods every extracted file, often twice): skip the change.
            let m = stat().map_err(|e| io_errno(&e))?;
            if m.mode() & 0o7777 == mode & 0o7777 {
                current = Some(m);
            } else {
                let mode = (mode & 0o7777) as libc::mode_t;
                let rc = match fd {
                    Some(fd) => unsafe { libc::fchmod(fd, mode) },
                    None => unsafe { libc::chmod(c.as_ptr(), mode) },
                };
                if rc != 0 {
                    return Err(os_err());
                }
            }
        }
        if valid & (FATTR_UID | FATTR_GID) != 0 {
            let u = if valid & FATTR_UID != 0 { uid } else { u32::MAX };
            let g = if valid & FATTR_GID != 0 { gid } else { u32::MAX };
            // We run as the user, so ownership changes mostly fail (guest
            // root extracting a tarball, Linux group IDs): ignore EPERM like
            // Apple's share does, and the file stays the user's.
            let rc = match fd {
                Some(fd) => unsafe { libc::fchown(fd, u, g) },
                None => unsafe { libc::lchown(c.as_ptr(), u, g) },
            };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EPERM) {
                    return Err(io_errno(&err));
                }
            } else {
                current = None;
            }
        }
        if valid & FATTR_SIZE != 0 {
            let result = match &file {
                Some(f) => f.set_len(size),
                None => fs::OpenOptions::new().write(true).open(&path).and_then(|f| f.set_len(size)),
            };
            result.map_err(|e| io_errno(&e))?;
            current = None;
        }
        if valid & (FATTR_ATIME | FATTR_MTIME | FATTR_ATIME_NOW | FATTR_MTIME_NOW) != 0 {
            let ts = |set: bool, now: bool, sec: u64, nsec: u32| libc::timespec {
                tv_sec: sec as libc::time_t,
                tv_nsec: if now {
                    libc::UTIME_NOW
                } else if set {
                    nsec as libc::c_long
                } else {
                    libc::UTIME_OMIT
                },
            };
            let times = [
                ts(valid & FATTR_ATIME != 0, valid & FATTR_ATIME_NOW != 0, atime, atimensec),
                ts(valid & FATTR_MTIME != 0, valid & FATTR_MTIME_NOW != 0, mtime, mtimensec),
            ];
            let rc = match fd {
                Some(fd) => unsafe { libc::futimens(fd, times.as_ptr()) },
                None => unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) },
            };
            if rc != 0 {
                return Err(os_err());
            }
            current = None;
        }
        let m = match current {
            Some(m) => m,
            None => stat().map_err(|e| io_errno(&e))?,
        };
        crate::watch::guest_wrote(&path, &m);
        Ok(self.attr_reply(nodeid, &m))
    }

    fn readlink(&self, nodeid: u64) -> Reply {
        let target = fs::read_link(self.path(nodeid)?).map_err(|e| io_errno(&e))?;
        Ok(target.as_os_str().as_bytes().to_vec())
    }

    fn symlink(&self, parent: u64, name: &[u8], target: &[u8]) -> Reply {
        let path = self.child(parent, name)?;
        std::os::unix::fs::symlink(OsStr::from_bytes(target), &path).map_err(|e| io_errno(&e))?;
        self.entry_noted(path)
    }

    fn mkdir(&self, parent: u64, name: &[u8], mode: u32) -> Reply {
        let path = self.child(parent, name)?;
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| lerr::EINVAL)?;
        if unsafe { libc::mkdir(c.as_ptr(), (mode & 0o7777) as libc::mode_t) } != 0 {
            return Err(io_errno(&std::io::Error::last_os_error()));
        }
        self.entry_noted(path)
    }

    fn remove(&self, parent: u64, name: &[u8], dir: bool) -> Reply {
        let path = self.child(parent, name)?;
        let ino = fs::symlink_metadata(&path).map(|m| m.ino()).ok();
        if dir { fs::remove_dir(&path) } else { fs::remove_file(&path) }.map_err(|e| io_errno(&e))?;
        crate::watch::guest_removed(&path);
        // A removed file may still be open in the guest: keep its node (and
        // open file) but never reopen the name, which may get a new file.
        if let Some(ino) = ino {
            self.forget_name(ino, &path);
        }
        Ok(Vec::new())
    }

    fn rename(&self, olddir: u64, old: &[u8], newdir: u64, new: &[u8]) -> Reply {
        let (from, to) = (self.child(olddir, old)?, self.child(newdir, new)?);
        let replaced = fs::symlink_metadata(&to).map(|m| m.ino()).ok();
        fs::rename(&from, &to).map_err(|e| io_errno(&e))?;
        crate::watch::guest_removed(&from);
        if let Ok(m) = fs::symlink_metadata(&to) {
            crate::watch::guest_wrote(&to, &m);
        }
        // Whatever the new name replaced is now nameless: it must never be
        // reopened by that path (it would be the moved file).
        if let Some(ino) = replaced {
            self.forget_name(ino, &to);
        }
        let mut nodes = self.nodes.lock().unwrap();
        // Path-keyed nodes under the old name move with it.
        for node in nodes.values_mut() {
            if let Ok(rest) = node.path.strip_prefix(&from) {
                node.path = if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) };
            }
        }
        Ok(Vec::new())
    }

    fn link(&self, old: u64, newdir: u64, name: &[u8]) -> Reply {
        let (from, to) = (self.path(old)?, self.child(newdir, name)?);
        fs::hard_link(&from, &to).map_err(|e| io_errno(&e))?;
        self.entry_noted(to)
    }

    fn new_handle(&self, handle: Handle) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, handle);
        fh
    }

    fn handle_file(&self, fh: u64) -> Option<Arc<fs::File>> {
        match self.handles.lock().unwrap().get(&fh) {
            Some(Handle::File(f)) => Some(f.clone()),
            _ => None,
        }
    }

    fn open_file(path: &Path, flags: u32, mode: u32) -> Result<fs::File, i32> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| lerr::EINVAL)?;
        let mut mac = mac_open_flags(flags);
        // With writeback caching the kernel may read from write-only files.
        if mac & libc::O_ACCMODE == libc::O_WRONLY {
            mac = (mac & !libc::O_ACCMODE) | libc::O_RDWR;
        }
        // O_APPEND would make our positioned writes append; the kernel sends
        // the right offsets itself.
        mac &= !libc::O_APPEND;
        let fd = unsafe { libc::open(c.as_ptr(), mac, mode as libc::c_uint) };
        if fd < 0 {
            return Err(io_errno(&std::io::Error::last_os_error()));
        }
        Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) })
    }

    fn open_out(fh: u64) -> Vec<u8> {
        let mut out = Out::default();
        out.u64(fh).u32(FOPEN_KEEP_CACHE).i32(0);
        out.0
    }

    fn open(&self, nodeid: u64, flags: u32) -> Reply {
        let file = Self::open_file(&self.path(nodeid)?, flags & !0o100, 0)?;
        let fh = self.new_handle(Handle::File(Arc::new(file)));
        Ok(Self::open_out(fh))
    }

    fn create(&self, parent: u64, name: &[u8], flags: u32, mode: u32) -> Reply {
        let path = self.child(parent, name)?;
        let file = Arc::new(Self::open_file(&path, flags | 0o100, mode & 0o7777)?);
        let m = file.metadata().map_err(|e| io_errno(&e))?;
        crate::watch::guest_wrote(&path, &m);
        let mut body = self.entry_with(path.clone(), &m)?;
        // A no-open kernel never sends RELEASE, even for created files: keep
        // the file as the node's (closed when evicted) and reply fh 0.
        let fh = if self.no_open.load(Ordering::Relaxed) {
            let nodeid = u64::from_le_bytes(body[0..8].try_into().unwrap());
            self.cache_file(nodeid, &path, file);
            0
        } else {
            self.new_handle(Handle::File(file))
        };
        body.extend_from_slice(&Self::open_out(fh));
        Ok(body)
    }

    /// The file for a read or write: the open handle, or with no-open the
    /// node's own file, opened read-write (read-only if that's all we may).
    fn file_for(&self, nodeid: u64, fh: u64) -> Result<Arc<fs::File>, i32> {
        if fh != 0 {
            return self.handle_file(fh).ok_or(lerr::EBADF);
        }
        let path = {
            let nodes = self.nodes.lock().unwrap();
            let node = nodes.get(&nodeid).ok_or(lerr::ENOENT)?;
            if let Some(f) = &node.file {
                return Ok(f.clone());
            }
            node.path.clone()
        };
        let file = Arc::new(Self::open_file(&path, 2, 0).or_else(|_| Self::open_file(&path, 0, 0))?);
        Ok(self.cache_file(nodeid, &path, file))
    }

    /// Replace the node's cached file with one opened read-write.
    fn reopen_writable(&self, nodeid: u64) -> Result<Arc<fs::File>, i32> {
        let path = self.path(nodeid)?;
        let file = Arc::new(Self::open_file(&path, 2, 0)?);
        let old = self.nodes.lock().unwrap().get_mut(&nodeid).and_then(|n| n.file.replace(file.clone()));
        drop(old);
        Ok(file)
    }

    /// Keep `file` as the node's open file, unless another thread got there
    /// first (then that one is returned), closing the oldest past the cap.
    fn cache_file(&self, nodeid: u64, path: &Path, file: Arc<fs::File>) -> Arc<fs::File> {
        let mut nodes = self.nodes.lock().unwrap();
        let Some(node) = nodes.get_mut(&nodeid).filter(|n| n.path == path) else { return file };
        if let Some(f) = &node.file {
            return f.clone();
        }
        node.file = Some(file.clone());
        let mut cached = self.cached.lock().unwrap();
        cached.push_back(nodeid);
        let mut evicted = Vec::new();
        while cached.len() > MAX_CACHED_FILES {
            let Some(old) = cached.pop_front() else { break };
            // A nameless node's file can't be reopened: keep it.
            if let Some(f) = nodes.get_mut(&old).filter(|n| !n.path.as_os_str().is_empty()).and_then(|n| n.file.take()) {
                evicted.push(f);
            }
        }
        drop((cached, nodes));
        drop(evicted);
        file
    }

    fn read(&self, nodeid: u64, fh: u64, offset: u64, size: u32) -> Reply {
        let f = self.file_for(nodeid, fh)?;
        let mut buf = vec![0u8; size as usize];
        let mut done = 0;
        while done < buf.len() {
            match f.read_at(&mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_errno(&e)),
            }
        }
        buf.truncate(done);
        Ok(buf)
    }

    fn write(&self, nodeid: u64, fh: u64, offset: u64, data: &[u8]) -> Reply {
        let mut f = self.file_for(nodeid, fh)?;
        if let Err(err) = f.write_all_at(data, offset) {
            // The node's file was opened read-only (the file was read-only
            // then, and has been made writable since): reopen it.
            if fh != 0 || err.raw_os_error() != Some(libc::EBADF) {
                return Err(io_errno(&err));
            }
            f = self.reopen_writable(nodeid)?;
            f.write_all_at(data, offset).map_err(|e| io_errno(&e))?;
        }
        if let (Ok(path), Ok(m)) = (self.path(nodeid), f.metadata()) {
            crate::watch::guest_wrote(&path, &m);
        }
        let mut out = Out::default();
        out.u32(data.len() as u32).u32(0);
        Ok(out.0)
    }

    fn fsync(&self, nodeid: u64, fh: u64) -> Reply {
        // With no-open, fh is 0 and the data went through the node's file.
        self.file_for(nodeid, fh)?.sync_data().map_err(|e| io_errno(&e))?;
        Ok(Vec::new())
    }

    fn statfs(&self) -> Reply {
        let c = CString::new(self.root.as_os_str().as_bytes()).map_err(|_| lerr::EINVAL)?;
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
            return Err(io_errno(&std::io::Error::last_os_error()));
        }
        let mut out = Out::default();
        out.u64(s.f_blocks)
            .u64(s.f_bfree)
            .u64(s.f_bavail)
            .u64(s.f_files)
            .u64(s.f_ffree)
            .u32(s.f_bsize)
            .u32(255)
            .u32(s.f_bsize)
            .u32(0)
            .zeros(24);
        Ok(out.0)
    }

    fn opendir(&self, nodeid: u64) -> Reply {
        let path = self.path(nodeid)?;
        let mut entries = vec![(b".".to_vec(), nodeid, libc::DT_DIR as u32), (b"..".to_vec(), 0, libc::DT_DIR as u32)];
        for entry in fs::read_dir(&path).map_err(|e| io_errno(&e))?.flatten() {
            let kind = entry.file_type().map_or(0, |t| {
                if t.is_dir() {
                    libc::DT_DIR
                } else if t.is_symlink() {
                    libc::DT_LNK
                } else if t.is_file() {
                    libc::DT_REG
                } else {
                    libc::DT_UNKNOWN
                }
            }) as u32;
            let ino = entry.metadata().map_or(0, |m| m.ino());
            entries.push((entry.file_name().as_bytes().to_vec(), ino, kind));
        }
        let fh = self.new_handle(Handle::Dir(Arc::new(Dir { path, entries })));
        Ok(Self::open_out(fh))
    }

    /// READDIR, or READDIRPLUS which also returns each entry's attributes
    /// (and counts as a lookup), saving the kernel a LOOKUP per name.
    fn readdir(&self, fh: u64, offset: u64, size: u32, plus: bool) -> Reply {
        let dir = match self.handles.lock().unwrap().get(&fh) {
            Some(Handle::Dir(d)) => d.clone(),
            _ => return Err(lerr::EBADF),
        };
        let mut out = Out::default();
        for (i, (name, ino, kind)) in dir.entries.iter().enumerate().skip(offset as usize) {
            let len = 24 + name.len();
            let padded = (len + 7) & !7;
            let need = padded + if plus { 128 } else { 0 };
            if out.0.len() + need > size as usize {
                break;
            }
            if plus {
                let entry = if name != b"." && name != b".." {
                    self.entry(dir.path.join(OsStr::from_bytes(name))).ok()
                } else {
                    None
                };
                match entry {
                    Some(e) => out.0.extend_from_slice(&e),
                    // "." / ".." or a vanished name: a zero entry (nodeid 0)
                    // tells the kernel not to cache anything.
                    None => {
                        out.zeros(128);
                    }
                }
            }
            out.u64(*ino).u64(i as u64 + 1).u32(name.len() as u32).u32(*kind);
            out.0.extend_from_slice(name);
            out.zeros(padded - len);
        }
        Ok(out.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(opcode: u32, nodeid: u64, body: &[u8]) -> Vec<u8> {
        let mut out = Out::default();
        out.u32((40 + body.len()) as u32).u32(opcode).u64(7).u64(nodeid).u32(501).u32(20).u32(1).u16(0).u16(0);
        out.0.extend_from_slice(body);
        out.0
    }

    fn reply(server: &Server, opcode: u32, nodeid: u64, body: &[u8]) -> (i32, Vec<u8>) {
        let r = server.handle(&header(opcode, nodeid, body)).unwrap();
        let error = i32::from_le_bytes(r[4..8].try_into().unwrap());
        assert_eq!(u64::from_le_bytes(r[8..16].try_into().unwrap()), 7, "unique echoed");
        (error, r[16..].to_vec())
    }

    fn name(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("berth-fuse-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn create_write_read_rename_unlink() {
        let dir = tempdir("rw");
        let s = Server::new(dir.clone(), CachePolicy::default());

        // INIT
        let mut init = Out::default();
        init.u32(7).u32(41).u32(131072).u32(FUSE_ASYNC_READ | FUSE_WRITEBACK_CACHE);
        let (err, body) = reply(&s, op::INIT, 0, &init.0);
        assert_eq!(err, 0);
        assert_eq!(body.len(), 64);
        assert_eq!(u32::from_le_bytes(body[4..8].try_into().unwrap()), 31, "minor negotiated down");

        // CREATE "a.txt" with O_WRONLY|O_CREAT|O_TRUNC
        let mut create = Out::default();
        create.u32(0o1 | 0o100 | 0o1000).u32(0o644).u32(0o022).u32(0);
        create.0.extend_from_slice(&name("a.txt"));
        let (err, body) = reply(&s, op::CREATE, FUSE_ROOT_ID, &create.0);
        assert_eq!(err, 0);
        let nodeid = u64::from_le_bytes(body[0..8].try_into().unwrap());
        let fh = u64::from_le_bytes(body[128..136].try_into().unwrap());

        // WRITE "hello" at 0
        let mut write = Out::default();
        write.u64(fh).u64(0).u32(5).u32(0).u64(0).u32(0).u32(0);
        write.0.extend_from_slice(b"hello");
        let (err, body) = reply(&s, op::WRITE, nodeid, &write.0);
        assert_eq!((err, u32::from_le_bytes(body[0..4].try_into().unwrap())), (0, 5));
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"hello");

        // READ it back through the handle
        let mut read = Out::default();
        read.u64(fh).u64(1).u32(100).u32(0).u64(0).u32(0).u32(0);
        assert_eq!(reply(&s, op::READ, nodeid, &read.0), (0, b"ello".to_vec()));

        // RENAME a.txt -> b.txt, then LOOKUP b.txt returns the same node
        let mut rename = Out::default();
        rename.u64(FUSE_ROOT_ID);
        rename.0.extend_from_slice(&name("a.txt"));
        rename.0.extend_from_slice(&name("b.txt"));
        assert_eq!(reply(&s, op::RENAME, FUSE_ROOT_ID, &rename.0).0, 0);
        let (err, body) = reply(&s, op::LOOKUP, FUSE_ROOT_ID, &name("b.txt"));
        assert_eq!((err, u64::from_le_bytes(body[0..8].try_into().unwrap())), (0, nodeid));

        // Missing names map to Linux ENOENT; removing a non-empty dir maps
        // macOS ENOTEMPTY (66) to Linux 39.
        let (err, body) = reply(&s, op::LOOKUP, FUSE_ROOT_ID, &name("nope"));
        assert_eq!((err, u64::from_le_bytes(body[0..8].try_into().unwrap())), (0, 0), "negative entry");
        let plain = Server::new(dir.clone(), CachePolicy { negative: Duration::ZERO, ..CachePolicy::default() });
        assert_eq!(reply(&plain, op::LOOKUP, FUSE_ROOT_ID, &name("nope")).0, -lerr::ENOENT);
        let mut mkdir = Out::default();
        mkdir.u32(0o755).u32(0);
        mkdir.0.extend_from_slice(&name("d"));
        assert_eq!(reply(&s, op::MKDIR, FUSE_ROOT_ID, &mkdir.0).0, 0);
        fs::write(dir.join("d/x"), "x").unwrap();
        assert_eq!(reply(&s, op::RMDIR, FUSE_ROOT_ID, &name("d")).0, -39);

        assert_eq!(reply(&s, op::UNLINK, FUSE_ROOT_ID, &name("b.txt")).0, 0);
        assert!(!dir.join("b.txt").exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn readdir_lists_entries_with_padding() {
        let dir = tempdir("rd");
        fs::write(dir.join("one"), "").unwrap();
        fs::create_dir(dir.join("two")).unwrap();
        let s = Server::new(dir.clone(), CachePolicy::default());
        let (err, body) = reply(&s, op::OPENDIR, FUSE_ROOT_ID, &[0; 8]);
        assert_eq!(err, 0);
        let fh = u64::from_le_bytes(body[0..8].try_into().unwrap());
        let mut read = Out::default();
        read.u64(fh).u64(0).u32(4096).u32(0).u64(0).u32(0).u32(0);
        let (err, body) = reply(&s, op::READDIR, FUSE_ROOT_ID, &read.0);
        assert_eq!(err, 0);
        let mut names = Vec::new();
        let mut pos = 0;
        while pos < body.len() {
            let len = u32::from_le_bytes(body[pos + 16..pos + 20].try_into().unwrap()) as usize;
            names.push(String::from_utf8_lossy(&body[pos + 24..pos + 24 + len]).into_owned());
            pos += (24 + len + 7) & !7;
        }
        names.sort();
        assert_eq!(names, [".", "..", "one", "two"]);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn readdirplus_and_no_open() {
        let dir = tempdir("plus");
        fs::write(dir.join("f"), "data").unwrap();
        let s = Server::new(dir.clone(), CachePolicy::default());
        // A kernel that supports no-open: OPEN is refused, reads use fh 0.
        let mut init = Out::default();
        init.u32(7).u32(41).u32(131072).u32(FUSE_NO_OPEN_SUPPORT);
        assert_eq!(reply(&s, op::INIT, 0, &init.0).0, 0);

        let (_, body) = reply(&s, op::OPENDIR, FUSE_ROOT_ID, &[0; 8]);
        let fh = u64::from_le_bytes(body[0..8].try_into().unwrap());
        let mut read = Out::default();
        read.u64(fh).u64(0).u32(4096).u32(0).u64(0).u32(0).u32(0);
        let (err, body) = reply(&s, op::READDIRPLUS, FUSE_ROOT_ID, &read.0);
        assert_eq!(err, 0);
        // Entries: ".", "..", "f" each with a 128-byte entry_out before the dirent.
        let mut pos = 0;
        let mut f_node = 0;
        while pos < body.len() {
            let nodeid = u64::from_le_bytes(body[pos..pos + 8].try_into().unwrap());
            let d = pos + 128;
            let len = u32::from_le_bytes(body[d + 16..d + 20].try_into().unwrap()) as usize;
            if &body[d + 24..d + 24 + len] == b"f" {
                f_node = nodeid;
            }
            pos = d + ((24 + len + 7) & !7);
        }
        assert_ne!(f_node, 0, "f came with a real entry");

        assert_eq!(reply(&s, op::OPEN, f_node, &[0; 8]).0, -lerr::ENOSYS);
        let mut read = Out::default();
        read.u64(0).u64(0).u32(100).u32(0).u64(0).u32(0).u32(0);
        assert_eq!(reply(&s, op::READ, f_node, &read.0), (0, b"data".to_vec()));
        assert_eq!(reply(&s, op::FLUSH, f_node, &[0; 24]).0, -lerr::ENOSYS);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn forget_has_no_reply_and_drops_node() {
        let dir = tempdir("fg");
        fs::write(dir.join("f"), "").unwrap();
        let s = Server::new(dir.clone(), CachePolicy::default());
        let (_, body) = reply(&s, op::LOOKUP, FUSE_ROOT_ID, &name("f"));
        let nodeid = u64::from_le_bytes(body[0..8].try_into().unwrap());
        assert!(s.nodes.lock().unwrap().contains_key(&nodeid));
        assert!(s.handle(&header(op::FORGET, nodeid, &1u64.to_le_bytes())).is_none());
        assert!(!s.nodes.lock().unwrap().contains_key(&nodeid));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn serves_from_many_threads() {
        let dir = tempdir("mt");
        let s = Server::new(dir.clone(), CachePolicy::default());
        std::thread::scope(|scope| {
            for t in 0..8 {
                let s = &s;
                scope.spawn(move || {
                    for i in 0..50 {
                        let file = format!("t{t}-{i}");
                        let mut create = Out::default();
                        create.u32(0o2 | 0o100).u32(0o644).u32(0).u32(0);
                        create.0.extend_from_slice(&name(&file));
                        let (err, body) = reply(s, op::CREATE, FUSE_ROOT_ID, &create.0);
                        assert_eq!(err, 0);
                        let nodeid = u64::from_le_bytes(body[0..8].try_into().unwrap());
                        let fh = u64::from_le_bytes(body[128..136].try_into().unwrap());
                        let mut write = Out::default();
                        write.u64(fh).u64(0).u32(file.len() as u32).u32(0).u64(0).u32(0).u32(0);
                        write.0.extend_from_slice(file.as_bytes());
                        assert_eq!(reply(s, op::WRITE, nodeid, &write.0).0, 0);
                        let mut read = Out::default();
                        read.u64(fh).u64(0).u32(100).u32(0).u64(0).u32(0).u32(0);
                        assert_eq!(reply(s, op::READ, nodeid, &read.0), (0, file.clone().into_bytes()));
                        assert_eq!(reply(s, op::RELEASE, nodeid, &fh.to_le_bytes()).0, 0);
                        if i % 2 == 0 {
                            assert_eq!(reply(s, op::UNLINK, FUSE_ROOT_ID, &name(&file)).0, 0);
                        }
                    }
                });
            }
        });
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 8 * 25);
        assert!(s.handles.lock().unwrap().is_empty(), "every handle released");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn no_open_create_keeps_no_handles_and_caps_open_files() {
        let dir = tempdir("cap");
        let s = Server::new(dir.clone(), CachePolicy::default());
        let mut init = Out::default();
        init.u32(7).u32(41).u32(131072).u32(FUSE_NO_OPEN_SUPPORT);
        assert_eq!(reply(&s, op::INIT, 0, &init.0).0, 0);
        for i in 0..MAX_CACHED_FILES + 100 {
            let mut create = Out::default();
            create.u32(0o1 | 0o100).u32(0o644).u32(0).u32(0);
            create.0.extend_from_slice(&name(&format!("f{i}")));
            let (err, body) = reply(&s, op::CREATE, FUSE_ROOT_ID, &create.0);
            assert_eq!(err, 0);
            assert_eq!(u64::from_le_bytes(body[128..136].try_into().unwrap()), 0, "fh 0: no RELEASE will come");
            let nodeid = u64::from_le_bytes(body[0..8].try_into().unwrap());
            let mut write = Out::default();
            write.u64(0).u64(0).u32(1).u32(0).u64(0).u32(0).u32(0);
            write.0.push(b'x');
            assert_eq!(reply(&s, op::WRITE, nodeid, &write.0).0, 0);
        }
        assert!(s.handles.lock().unwrap().is_empty());
        let open = s.nodes.lock().unwrap().values().filter(|n| n.file.is_some()).count();
        assert_eq!(open, MAX_CACHED_FILES);
        assert_eq!(fs::read(dir.join("f0")).unwrap(), b"x", "evicted files were written");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn removed_on_the_mac_is_stale_not_missing() {
        let dir = tempdir("stale");
        fs::create_dir_all(dir.join("vendor/old")).unwrap();
        let s = Server::new(dir.clone(), CachePolicy::default());
        let (_, body) = reply(&s, op::LOOKUP, FUSE_ROOT_ID, &name("vendor"));
        let vendor = u64::from_le_bytes(body[0..8].try_into().unwrap());
        // A missing name in a directory that exists is just missing...
        let (err, body) = reply(&s, op::LOOKUP, vendor, &name("nope"));
        assert_eq!((err, u64::from_le_bytes(body[0..8].try_into().unwrap())), (0, 0));
        // ...but once the directory itself is removed behind the guest's
        // back, anything through it is stale.
        fs::remove_dir_all(dir.join("vendor")).unwrap();
        let mut mkdir = Out::default();
        mkdir.u32(0o755).u32(0);
        mkdir.0.extend_from_slice(&name("composer"));
        assert_eq!(reply(&s, op::MKDIR, vendor, &mkdir.0).0, -lerr::ESTALE);
        assert_eq!(reply(&s, op::LOOKUP, vendor, &name("nope")).0, -lerr::ESTALE);
        assert_eq!(reply(&s, op::GETATTR, vendor, &[0; 16]).0, -lerr::ESTALE);
        fs::remove_dir_all(dir).ok();
    }

    fn setattr_body(valid: u32, size: u64, mtime: u64, mode: u32) -> Vec<u8> {
        let mut b = Out::default();
        b.u32(valid).u32(0).u64(0).u64(size).u64(0).u64(0).u64(mtime).u64(0).u32(0).u32(0).u32(0).u32(mode).u32(0).u32(0).u32(0).u32(0);
        b.0
    }

    #[test]
    fn setattr_through_the_open_file() {
        let dir = tempdir("setattr");
        let s = Server::new(dir.clone(), CachePolicy::default());
        let mut init = Out::default();
        init.u32(7).u32(41).u32(131072).u32(FUSE_NO_OPEN_SUPPORT);
        reply(&s, op::INIT, 0, &init.0);
        let mut create = Out::default();
        create.u32(0o1 | 0o100).u32(0o644).u32(0).u32(0);
        create.0.extend_from_slice(&name("f"));
        let (_, body) = reply(&s, op::CREATE, FUSE_ROOT_ID, &create.0);
        let nodeid = u64::from_le_bytes(body[0..8].try_into().unwrap());
        let mut write = Out::default();
        write.u64(0).u64(0).u32(5).u32(0).u64(0).u32(0).u32(0);
        write.0.extend_from_slice(b"hello");
        reply(&s, op::WRITE, nodeid, &write.0);
        let mode_of = |body: &[u8]| u32::from_le_bytes(body[16 + 60..16 + 64].try_into().unwrap()) & 0o7777;

        // Same mode: nothing changes, the reply still carries the attributes.
        let (err, body) = reply(&s, op::SETATTR, nodeid, &setattr_body(FATTR_MODE, 0, 0, 0o100644));
        assert_eq!((err, mode_of(&body)), (0, 0o644));
        let (err, body) = reply(&s, op::SETATTR, nodeid, &setattr_body(FATTR_MODE, 0, 0, 0o100600));
        assert_eq!((err, mode_of(&body)), (0, 0o600));
        assert_eq!(fs::metadata(dir.join("f")).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(reply(&s, op::SETATTR, nodeid, &setattr_body(FATTR_SIZE, 2, 0, 0)).0, 0);
        assert_eq!(fs::read(dir.join("f")).unwrap(), b"he");
        assert_eq!(reply(&s, op::SETATTR, nodeid, &setattr_body(FATTR_MTIME, 0, 1_000_000_000, 0)).0, 0);
        assert_eq!(fs::metadata(dir.join("f")).unwrap().mtime(), 1_000_000_000);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn replaced_and_removed_files_stay_themselves() {
        let dir = tempdir("orphan");
        fs::write(dir.join("config.json"), "old").unwrap();
        fs::write(dir.join("tmp"), "new").unwrap();
        let s = Server::new(dir.clone(), CachePolicy::default());
        let mut init = Out::default();
        init.u32(7).u32(41).u32(131072).u32(FUSE_NO_OPEN_SUPPORT);
        reply(&s, op::INIT, 0, &init.0);
        let node = |n: &str| {
            let (_, body) = reply(&s, op::LOOKUP, FUSE_ROOT_ID, &name(n));
            u64::from_le_bytes(body[0..8].try_into().unwrap())
        };
        let read = |nodeid: u64| {
            let mut r = Out::default();
            r.u64(0).u64(0).u32(100).u32(0).u64(0).u32(0).u32(0);
            reply(&s, op::READ, nodeid, &r.0)
        };
        let old = node("config.json");
        assert_eq!(read(old), (0, b"old".to_vec()), "opens and caches its file");
        // Saving by rename over it: the old node keeps its own contents and
        // never turns into the new file.
        let mut rename = Out::default();
        rename.u64(FUSE_ROOT_ID);
        rename.0.extend_from_slice(&name("tmp"));
        rename.0.extend_from_slice(&name("config.json"));
        assert_eq!(reply(&s, op::RENAME, FUSE_ROOT_ID, &rename.0).0, 0);
        assert_eq!(read(old), (0, b"old".to_vec()));
        assert_eq!(reply(&s, op::GETATTR, old, &[0; 16]).0, 0, "fstat of the open file");
        // Removed while open: still readable and syncable.
        let new = node("config.json");
        assert_eq!(read(new), (0, b"new".to_vec()));
        assert_eq!(reply(&s, op::UNLINK, FUSE_ROOT_ID, &name("config.json")).0, 0);
        assert_eq!(read(new), (0, b"new".to_vec()));
        assert_eq!(reply(&s, op::FSYNC, new, &[0; 16]).0, 0);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn errno_and_flags_translate() {
        assert_eq!(linux_errno(libc::ENOENT), 2);
        assert_eq!(linux_errno(libc::ENOTEMPTY), 39);
        assert_eq!(linux_errno(libc::ENAMETOOLONG), 36);
        let f = mac_open_flags(0o2 | 0o100 | 0o1000);
        assert_eq!(f & libc::O_ACCMODE, libc::O_RDWR);
        assert!(f & libc::O_CREAT != 0 && f & libc::O_TRUNC != 0);
        // arm64: O_LARGEFILE (always set on 64-bit) and O_DIRECT mean nothing
        // on macOS; O_DIRECTORY and O_NOFOLLOW have arm64's values.
        assert_eq!(mac_open_flags(0o400000 | 0o200000), libc::O_CLOEXEC);
        assert!(mac_open_flags(0o40000) & libc::O_DIRECTORY != 0);
        assert!(mac_open_flags(0o100000) & libc::O_NOFOLLOW != 0);
    }
}
