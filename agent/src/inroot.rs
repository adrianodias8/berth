//! Files inside a machine's root filesystem, opened as the machine would see
//! them: every path, symlinks and `..` included, resolves within that root
//! (openat2 RESOLVE_IN_ROOT). The agent runs as the VM's root, and a
//! machine's root user controls the symlinks there; following them from the
//! VM would let it point our writes at the VM's own files.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

/// Linux's openat2 (the same number on every architecture).
const SYS_OPENAT2: libc::c_long = 437;

fn openat2(dir: &OwnedFd, path: &str, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let c = CString::new(path.trim_start_matches('/'))?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags | libc::O_CLOEXEC) as u64;
    how.mode = if flags & libc::O_CREAT != 0 { mode as u64 } else { 0 };
    how.resolve = libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS;
    let fd = unsafe {
        libc::syscall(SYS_OPENAT2, dir.as_raw_fd(), c.as_ptr(), &how as *const libc::open_how, size_of::<libc::open_how>())
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// A machine's root, to open paths in.
pub struct Root(OwnedFd);

impl Root {
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Root(File::open(root.as_ref())?.into()))
    }

    pub fn read(&self, path: &str, limit: u64) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        File::from(openat2(&self.0, path, libc::O_RDONLY, 0)?).take(limit).read_to_end(&mut out)?;
        Ok(out)
    }

    pub fn write(&self, path: &str, contents: &[u8]) -> io::Result<()> {
        let mut file = File::from(openat2(&self.0, path, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644)?);
        file.write_all(contents)
    }

    /// The folder holding `path` (created as needed) and its last component.
    fn parent<'a>(&self, path: &'a str) -> io::Result<(OwnedFd, &'a str)> {
        let path = path.trim_start_matches('/');
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut sofar = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            sofar.push('/');
            sofar.push_str(part);
            if let Err(err) = openat2(&self.0, &sofar, libc::O_PATH | libc::O_DIRECTORY, 0) {
                if err.kind() != io::ErrorKind::NotFound {
                    return Err(err);
                }
                let (up, last) = self.parent(&sofar)?;
                let c = CString::new(last)?;
                if unsafe { libc::mkdirat(up.as_raw_fd(), c.as_ptr(), 0o755) } != 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::AlreadyExists {
                        return Err(err);
                    }
                }
            }
        }
        let dirfd = openat2(&self.0, if dir.is_empty() { "." } else { dir }, libc::O_PATH | libc::O_DIRECTORY, 0)?;
        Ok((dirfd, name))
    }

    /// `path` becomes a symlink to `target` (whatever was there goes).
    pub fn symlink(&self, target: &str, path: &str) -> io::Result<()> {
        let (dir, name) = self.parent(path)?;
        let (t, n) = (CString::new(target)?, CString::new(name)?);
        unsafe { libc::unlinkat(dir.as_raw_fd(), n.as_ptr(), 0) };
        if unsafe { libc::symlinkat(t.as_ptr(), dir.as_raw_fd(), n.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Write `path`, creating its folders.
    pub fn write_creating(&self, path: &str, contents: &[u8]) -> io::Result<()> {
        self.parent(path)?;
        self.write(path, contents)
    }

    pub fn read_link(&self, path: &str) -> io::Result<String> {
        let (dir, name) = self.parent(path)?;
        let n = CString::new(name)?;
        let mut buf = vec![0u8; 4096];
        let len = unsafe { libc::readlinkat(dir.as_raw_fd(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if len < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(len as usize);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlinks_stay_inside() {
        let dir = std::env::temp_dir().join(format!("berth-inroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("root/etc")).unwrap();
        std::fs::write(dir.join("outside"), "secret").unwrap();
        // A machine pointing its files at the VM's.
        std::os::unix::fs::symlink(dir.join("outside"), dir.join("root/etc/abs")).unwrap();
        std::os::unix::fs::symlink("../../outside", dir.join("root/etc/rel")).unwrap();
        let root = Root::new(dir.join("root")).unwrap();
        assert!(root.read("/etc/abs", 100).is_err(), "absolute links resolve inside");
        assert!(root.read("/etc/rel", 100).is_err(), "so does ..");
        root.write("/etc/rel", b"x").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("outside")).unwrap(), "secret", "untouched");
        root.write_creating("/etc/profile.d/berth.sh", b"hi").unwrap();
        assert_eq!(std::fs::read(dir.join("root/etc/profile.d/berth.sh")).unwrap(), b"hi");
        root.symlink("/opt/agent", "/usr/local/bin/mac").unwrap();
        assert_eq!(root.read_link("/usr/local/bin/mac").unwrap(), "/opt/agent");
        std::fs::remove_dir_all(dir).ok();
    }
}
