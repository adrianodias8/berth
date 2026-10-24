//! Connections to the Mac's loopback over berth's tunnel device instead of
//! vsock (the host side, and why, is in src/fuse/tunnel.rs). Each connection
//! is two stream files in the mounted share: `tcp-<port>-<id>-down`
//! (opening it connects; reads are the Mac's data, 0 at its end) and
//! `tcp-<port>-<id>-up` (writes go to the Mac; closing it ends our side).
//! Without the device (Apple's share, older macOS) callers use vsock.
//!
//! The host answers an idle read with EAGAIN every couple of seconds (a
//! waiting FUSE read can't be interrupted), and a write may come back short
//! or EAGAIN when the VM is snapshotted: both are retried here. Each idle
//! connection keeps a request in the device's queue, so past MAX_CONNS
//! connections new ones go over vsock.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const MOUNT: &str = "/run/berth-tunnel";
static MOUNTED: OnceLock<bool> = OnceLock::new();
const MAX_CONNS: usize = 48;
static CONNS: AtomicUsize = AtomicUsize::new(0);

/// A tunnel connection's place under MAX_CONNS.
pub struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        CONNS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Mount the tunnel share (polling driver if it's there, else the built-in).
pub fn mount() -> bool {
    *MOUNTED.get_or_init(|| {
        if mounted() {
            return true; // the agent restarted
        }
        let _ = std::fs::create_dir_all(MOUNT);
        let _ = std::process::Command::new("modprobe").args(["-q", "virtiofs_poll"]).status();
        let (tag, dir) = (c"berth-tunnel", c"/run/berth-tunnel");
        for fstype in [c"virtiofs_poll", c"virtiofs"] {
            if unsafe { libc::mount(tag.as_ptr(), dir.as_ptr(), fstype.as_ptr(), 0, std::ptr::null()) } == 0 {
                eprintln!("berth-agent: tunnel: mounted ({})", fstype.to_string_lossy());
                return true;
            }
        }
        false
    })
}

/// Whether the share is already mounted at MOUNT (field 5 of mountinfo).
fn mounted() -> bool {
    std::fs::read_to_string("/proc/self/mountinfo")
        .is_ok_and(|info| info.lines().any(|l| l.split(' ').nth(4) == Some(MOUNT)))
}

/// A connection to the Mac's 127.0.0.1:<port> (or ::1) through the tunnel,
/// or None to use vsock.
pub fn connect(port: u16) -> Option<io::Result<(File, File, Slot)>> {
    if !MOUNTED.get().copied().unwrap_or(false) {
        return None;
    }
    if CONNS.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
        CONNS.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    let slot = Slot;
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
        Ok((down, up, slot))
    })())
}

fn retry(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}

/// Copy between a guest connection and a tunnel connection until both ends
/// are done.
pub fn relay(conn: TcpStream, down: File, up: File, slot: Slot) -> io::Result<()> {
    let _slot = slot;
    let mut to_mac = conn.try_clone()?;
    let upstream = std::thread::spawn(move || {
        let mut up = up;
        let mut buf = vec![0u8; 1 << 20];
        'conn: loop {
            let n = match to_mac.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut sent = 0;
            while sent < n {
                match up.write(&buf[sent..n]) {
                    Ok(0) => break 'conn,
                    Ok(m) => sent += m,
                    Err(e) if retry(&e) => {}
                    Err(_) => {
                        // The Mac's side is gone: stop the guest sending.
                        let _ = to_mac.shutdown(Shutdown::Both);
                        break 'conn;
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
            Ok(0) => break,
            Ok(n) => {
                if from_mac.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if retry(&e) => {}
            Err(_) => {
                // Reset rather than a clean end: the data didn't all arrive.
                let _ = from_mac.shutdown(Shutdown::Both);
                break;
            }
        }
    }
    let _ = from_mac.shutdown(Shutdown::Write);
    // The Mac's reading side is done; the upstream thread runs until the
    // guest side ends its data.
    drop(down);
    let _ = upstream.join();
    Ok(())
}
