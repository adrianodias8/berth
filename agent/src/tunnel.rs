//! Connections to the Mac's loopback over berth's tunnel device instead of
//! vsock (the host side, and why, is in src/fuse/tunnel.rs). Each connection
//! is two stream files in the mounted share: `tcp-<port>-<id>-down`
//! (opening it connects; reads are the Mac's data, 0 at its end) and
//! `tcp-<port>-<id>-up` (writes go to the Mac; closing it ends our side).
//! Without the device (Apple's share, older macOS) callers use vsock.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

const MOUNT: &str = "/run/berth-tunnel";
static MOUNTED: OnceLock<bool> = OnceLock::new();

/// Mount the tunnel share (polling driver if it's there, else the built-in).
pub fn mount() -> bool {
    *MOUNTED.get_or_init(|| {
        let _ = std::fs::create_dir_all(MOUNT);
        let _ = std::process::Command::new("modprobe").args(["-q", "virtiofs_poll"]).status();
        let (tag, dir) = (c"berth-tunnel", c"/run/berth-tunnel");
        for fstype in [c"virtiofs_poll", c"virtiofs"] {
            if unsafe { libc::mount(tag.as_ptr(), dir.as_ptr(), fstype.as_ptr(), 0, std::ptr::null()) } == 0 {
                eprintln!("berth-agent: tunnel: mounted ({})", fstype.to_string_lossy());
                return true;
            }
        }
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) {
            return true; // already mounted (agent restarted)
        }
        false
    })
}

/// A connection to the Mac's 127.0.0.1:<port> (or ::1) through the tunnel,
/// or None to use vsock.
pub fn connect(port: u16) -> Option<io::Result<(File, File)>> {
    if !MOUNTED.get().copied().unwrap_or(false) {
        return None;
    }
    // Unique across agent restarts: the host keys connections by this id.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if NEXT.load(Ordering::Relaxed) == 0 {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_micros() as u64) << 16;
        let _ = NEXT.compare_exchange(0, seed, Ordering::Relaxed, Ordering::Relaxed);
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = |way: &str| format!("{MOUNT}/tcp-{port}-{id}-{way}");
    Some((|| {
        let down = File::open(path("down"))?;
        let up = std::fs::OpenOptions::new().write(true).open(path("up"))?;
        Ok((down, up))
    })())
}

/// Copy between a guest connection and a tunnel connection until both ends
/// are done.
pub fn relay(conn: TcpStream, down: File, up: File) -> io::Result<()> {
    let mut to_mac = conn.try_clone()?;
    let upstream = std::thread::spawn(move || {
        let mut up = up;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match to_mac.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if up.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        // Dropping `up` closes it: the Mac sees the end of our data.
    });
    let (mut down, mut from_mac) = (down, conn);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match down.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if from_mac.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = from_mac.shutdown(Shutdown::Write);
    let _ = upstream.join();
    Ok(())
}
