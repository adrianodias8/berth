//! File system calls that never follow a symlink in any component of a path.
//!
//! The server works by path, and containers can change what's under a bind
//! mount at any moment: a directory swapped for a symlink between a LOOKUP
//! and its use would otherwise let a path lead outside the share (to
//! anything the user can reach). The guest kernel resolves symlinks itself
//! (READLINK), so the server never needs to follow one: every call here
//! fails with ELOOP instead. macOS 11+ has the flags for most calls
//! (`O_NOFOLLOW_ANY`, `AT_SYMLINK_NOFOLLOW_ANY`, `RENAME_NOFOLLOW_ANY`);
//! the rest work on the parent directory, opened the same way, plus a name.

use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

const AT_SYMLINK_NOFOLLOW_ANY: libc::c_int = 0x0800;
const RENAME_NOFOLLOW_ANY: libc::c_uint = 0x10;
/// Open a directory for searching only, so it needn't be readable.
const O_EXEC: libc::c_int = 0x4000_0000;

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// `stat` results, with the accessors the server uses.
pub struct Stat(libc::stat);

impl Stat {
    pub fn ino(&self) -> u64 {
        self.0.st_ino
    }
    pub fn size(&self) -> u64 {
        self.0.st_size as u64
    }
    pub fn blocks(&self) -> u64 {
        self.0.st_blocks as u64
    }
    pub fn atime(&self) -> (i64, u32) {
        (self.0.st_atime, self.0.st_atime_nsec as u32)
    }
    pub fn mtime(&self) -> (i64, u32) {
        (self.0.st_mtime, self.0.st_mtime_nsec as u32)
    }
    pub fn ctime(&self) -> (i64, u32) {
        (self.0.st_ctime, self.0.st_ctime_nsec as u32)
    }
    pub fn mode(&self) -> u32 {
        u32::from(self.0.st_mode)
    }
    pub fn nlink(&self) -> u32 {
        u32::from(self.0.st_nlink)
    }
    pub fn uid(&self) -> u32 {
        self.0.st_uid
    }
    pub fn gid(&self) -> u32 {
        self.0.st_gid
    }
    pub fn rdev(&self) -> u32 {
        self.0.st_rdev as u32
    }
    pub fn blksize(&self) -> u32 {
        self.0.st_blksize as u32
    }
    pub fn is_dir(&self) -> bool {
        self.mode() & u32::from(libc::S_IFMT) == u32::from(libc::S_IFDIR)
    }
    pub fn modified(&self) -> SystemTime {
        let (s, ns) = self.mtime();
        SystemTime::UNIX_EPOCH + Duration::new(s.max(0) as u64, ns)
    }
}

/// `lstat`: a symlink at the end is described, not followed.
pub fn lstat(path: &Path) -> io::Result<Stat> {
    let c = cstr(path)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstatat(libc::AT_FDCWD, c.as_ptr(), &mut st, AT_SYMLINK_NOFOLLOW_ANY) })?;
    Ok(Stat(st))
}

pub fn fstat(file: &fs::File) -> io::Result<Stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstat(file.as_raw_fd(), &mut st) })?;
    Ok(Stat(st))
}

/// `open(2)`, with `O_NOFOLLOW_ANY` added to `flags`.
pub fn open(path: &Path, flags: libc::c_int, mode: u32) -> io::Result<fs::File> {
    let c = cstr(path)?;
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC, mode as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

/// The directory containing `path` (opened refusing symlinks) and the name in it.
fn parent(path: &Path) -> io::Result<(OwnedFd, CString)> {
    let (dir, name) = match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => (dir, name),
        _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
    };
    let c = cstr(dir)?;
    let fd = unsafe { libc::open(c.as_ptr(), O_EXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let name = CString::new(name.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    Ok((unsafe { OwnedFd::from_raw_fd(fd) }, name))
}

pub fn mkdir(path: &Path, mode: u32) -> io::Result<()> {
    let (dir, name) = parent(path)?;
    check(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), (mode & 0o7777) as libc::mode_t) })
}

pub fn symlink(target: &OsStr, path: &Path) -> io::Result<()> {
    let (dir, name) = parent(path)?;
    let target = CString::new(target.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    check(unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) })
}

pub fn hard_link(from: &Path, to: &Path) -> io::Result<()> {
    let ((from_dir, from_name), (to_dir, to_name)) = (parent(from)?, parent(to)?);
    check(unsafe { libc::linkat(from_dir.as_raw_fd(), from_name.as_ptr(), to_dir.as_raw_fd(), to_name.as_ptr(), 0) })
}

pub fn read_link(path: &Path) -> io::Result<Vec<u8>> {
    let (dir, name) = parent(path)?;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let n = unsafe { libc::readlinkat(dir.as_raw_fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(buf)
}

pub fn remove(path: &Path, dir: bool) -> io::Result<()> {
    let c = cstr(path)?;
    let flags = AT_SYMLINK_NOFOLLOW_ANY | if dir { libc::AT_REMOVEDIR } else { 0 };
    check(unsafe { libc::unlinkat(libc::AT_FDCWD, c.as_ptr(), flags) })
}

pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    check(unsafe { libc::renameatx_np(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), RENAME_NOFOLLOW_ANY) })
}

pub fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    let c = cstr(path)?;
    check(unsafe { libc::fchmodat(libc::AT_FDCWD, c.as_ptr(), (mode & 0o7777) as libc::mode_t, AT_SYMLINK_NOFOLLOW_ANY) })
}

pub fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = cstr(path)?;
    check(unsafe { libc::fchownat(libc::AT_FDCWD, c.as_ptr(), uid, gid, AT_SYMLINK_NOFOLLOW_ANY) })
}

pub fn utimes(path: &Path, times: &[libc::timespec; 2]) -> io::Result<()> {
    let c = cstr(path)?;
    check(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), AT_SYMLINK_NOFOLLOW_ANY) })
}

/// A directory's entries as (name, inode, `DT_*` type), without "." and "..".
pub fn read_dir(path: &Path) -> io::Result<Vec<(Vec<u8>, u64, u32)>> {
    let dir = open(path, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let stream = unsafe { libc::fdopendir(std::os::fd::IntoRawFd::into_raw_fd(dir)) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut entries = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        let name = unsafe { std::ffi::CStr::from_ptr(entry.d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        entries.push((name.to_vec(), entry.d_ino, u32::from(entry.d_type)));
    }
    unsafe { libc::closedir(stream) };
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlinks_in_paths_are_refused() {
        let dir = std::env::temp_dir().join(format!("berth-nofollow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // The temp dir is under /var, itself a symlink to /private/var.
        let dir = fs::canonicalize(&dir).unwrap();
        fs::create_dir_all(dir.join("real")).unwrap();
        fs::create_dir_all(dir.join("outside")).unwrap();
        fs::write(dir.join("outside/secret"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("outside"), dir.join("link")).unwrap();
        let through = dir.join("link/secret");
        let eloop = |r: io::Result<()>| r.unwrap_err().raw_os_error() == Some(libc::ELOOP);
        assert_eq!(lstat(&through).err().and_then(|e| e.raw_os_error()), Some(libc::ELOOP));
        assert!(open(&through, libc::O_RDONLY, 0).is_err());
        assert!(eloop(chmod(&through, 0o777)));
        assert!(eloop(remove(&through, false)));
        assert!(mkdir(&dir.join("link/new"), 0o755).is_err());
        assert!(rename(&through, &dir.join("real/stolen")).is_err());
        assert!(read_dir(&dir.join("link")).is_err());
        assert!(fs::metadata(dir.join("outside/secret")).is_ok(), "untouched");
        // A symlink at the end is described, not followed.
        assert!(lstat(&dir.join("link")).is_ok());
        assert_eq!(read_link(&dir.join("link")).unwrap(), dir.join("outside").as_os_str().as_bytes());
        // Ordinary paths work.
        mkdir(&dir.join("real/sub"), 0o755).unwrap();
        symlink(OsStr::new("sub"), &dir.join("real/l")).unwrap();
        hard_link(&dir.join("outside/secret"), &dir.join("real/hard")).unwrap();
        let names: Vec<_> = read_dir(&dir.join("real")).unwrap().into_iter().map(|e| e.0).collect();
        assert_eq!(names.len(), 3);
        fs::remove_dir_all(dir).ok();
    }
}
