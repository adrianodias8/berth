//! berth guest agent. Listens on a vsock port; each connection starts with one
//! header line naming where to connect inside the guest, then carries raw bytes:
//!
//!   tcp 127.0.0.1:8080\n
//!   unix /run/docker.sock\n
//!   info domains\n          (replies with the domain table as JSON)
//!   info network\n          (replies with the guest address and Docker subnets)
//!   info memory\n           (replies with MemTotal/MemAvailable/PSI, see memory.rs)
//!   info listeners\n        (replies with host-network containers' listening ports, listeners.rs)
//!   k8s <action>\n          enable|disable|status|kubeconfig (k8s.rs)
//!   machine <action>\n      then JSON args until EOF: create|start|stop|delete|list (machines.rs)
//!   engine <action> [name]\n enable|disable podman|nerdctl, status (engines.rs)
//!   touch\n                 followed by newline-separated paths until EOF: raise
//!                           inotify events for files changed on the Mac
//!   sync\n                  flush filesystems (before a disk-only snapshot); replies "ok"
//!   clock <unix seconds>\n  set the clock if it's off by more than 50 ms (the VM's
//!                           clock stops while the Mac sleeps, and after restoring a
//!                           snapshot); replies with how far off it was, in seconds
//!
//! A failed connect just closes the stream. The agent also runs the container
//! domain controller (domains.rs), its mDNS responder (mdns.rs), DNS for
//! containers (dns.rs), host.docker.internal forwarding (hostnet.rs), web
//! access to domains: port detection and HTTPS (web.rs), and the watcher for
//! host-network containers' ports (listeners.rs).

mod boot;
mod dns;
mod docker;
mod domains;
mod engine;
mod engines;
mod hostnet;
mod ipv6;
mod k8s;
mod kmod;
mod listeners;
mod machines;
mod mdns;
mod memory;
mod web;

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, OnceLock};
use std::thread;

/// Must match `AGENT_VSOCK_PORT` on the host.
const PORT: u32 = 2400;
const MAX_HEADER: usize = 512;

static TABLE: OnceLock<domains::Shared> = OnceLock::new();

fn main() {
    let table = TABLE.get_or_init(|| Arc::new(Default::default()));
    hostnet::setup();
    // docker-ce before anything talks to Docker; the watchers retry anyway.
    thread::spawn(engine::ensure_docker_ce);
    let (t1, t2, t3) = (table.clone(), table.clone(), table.clone());
    web::run(table.clone());
    thread::spawn(memory::watch_pressure);
    thread::spawn(memory::ensure_kernel_args);
    thread::spawn(boot::tune);
    thread::spawn(boot::export_kernel);
    thread::spawn(kmod::run);
    thread::spawn(move || domains::run(t1));
    thread::spawn(move || mdns::run(t2));
    thread::spawn(move || dns::run(t3));
    thread::spawn(hostnet::run);
    thread::spawn(listeners::run);

    let listener = match vsock_listen(PORT) {
        Ok(fd) => fd,
        Err(err) => {
            eprintln!("berth-agent: listen on vsock {PORT}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!("berth-agent: listening on vsock port {PORT}");
    loop {
        match vsock_accept(&listener) {
            Ok(conn) => {
                thread::spawn(move || {
                    if let Err(err) = handle(conn) {
                        eprintln!("berth-agent: {err}");
                    }
                });
            }
            Err(err) => eprintln!("berth-agent: accept: {err}"),
        }
    }
}

fn handle(fd: OwnedFd) -> io::Result<()> {
    // vsock fds support the same read/write/shutdown calls as unix sockets.
    let mut host = UnixStream::from(fd);
    let header = read_header(&mut host)?;
    if header == "touch" {
        return touch_paths(host);
    }
    if header == "sync" {
        unsafe { libc::sync() };
        return host.write_all(b"ok");
    }
    if let Some(secs) = header.strip_prefix("clock ") {
        return set_clock(secs, host);
    }
    match header.split_once(' ') {
        Some(("tcp", addr)) => {
            let guest = TcpStream::connect(addr)?;
            guest.set_nodelay(true)?;
            splice(host, guest)
        }
        Some(("unix", path)) => splice(host, UnixStream::connect(path)?),
        Some(("k8s", action)) => k8s::handle(action, host),
        Some(("machine", action)) => machines::handle(action, host),
        Some(("engine", args)) => engines::handle(args, host),
        Some(("info", "nerdctl")) => {
            serde_json::to_writer(&mut host, &engines::nerdctl_containers()).map_err(io::Error::other)?;
            Ok(())
        }
        Some(("info", "listeners")) => {
            serde_json::to_writer(&mut host, &listeners::current()).map_err(io::Error::other)?;
            Ok(())
        }
        Some(("info", "memory")) => {
            serde_json::to_writer(&mut host, &memory::read()?).map_err(io::Error::other)?;
            Ok(())
        }
        Some(("info", "network")) => {
            let table = TABLE.get().unwrap().read().unwrap();
            let info = domains::NetworkInfo { gateway: table.guest_ip, subnets: table.subnets.clone() };
            drop(table);
            serde_json::to_writer(&mut host, &info).map_err(io::Error::other)?;
            Ok(())
        }
        Some(("info", "domains")) => {
            let entries = TABLE.get().unwrap().read().unwrap().entries.clone();
            serde_json::to_writer(&mut host, &entries).map_err(io::Error::other)?;
            Ok(())
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad header {header:?}"))),
    }
}

/// The VM's clock doesn't run while the Mac sleeps (and a restored snapshot
/// wakes up with the time it was saved at); NTP polls too rarely to notice.
fn set_clock(secs: &str, mut host: UnixStream) -> io::Result<()> {
    let mac: f64 = secs.trim().parse().map_err(|_| io::Error::other("bad time"))?;
    let ours = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    let drift = ours - mac;
    if drift.abs() > 0.05 {
        let ts = libc::timespec { tv_sec: mac.trunc() as _, tv_nsec: (mac.fract() * 1e9) as _ };
        if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    host.write_all(format!("{drift:.3}").as_bytes())
}

/// chmod each path to its current mode: a no-op that still raises IN_ATTRIB
/// for inotify watchers in the guest and refreshes virtiofs's cached
/// attributes. Missing paths (deleted on the Mac) are skipped; the host also
/// sends their parent directory.
fn touch_paths(mut host: UnixStream) -> io::Result<()> {
    let mut list = String::new();
    host.read_to_string(&mut list)?;
    for path in list.lines().filter(|l| !l.is_empty()) {
        let _ = touch(path);
    }
    Ok(())
}

/// chmod `path` to its current mode. We run as root and containers can
/// change what's under a bind mount, so the path is opened refusing symlinks
/// in any component (a swapped-in link to /etc/shadow can't be chmodded),
/// and the mode is read and set through that one handle.
fn touch(path: &str) -> io::Result<()> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    let c = std::ffi::CString::new(path).map_err(io::Error::other)?;
    let how = OpenHow { flags: (libc::O_PATH | libc::O_CLOEXEC) as u64, mode: 0, resolve: RESOLVE_NO_SYMLINKS };
    let fd = unsafe { libc::syscall(libc::SYS_openat2, libc::AT_FDCWD, c.as_ptr(), &how, std::mem::size_of::<OpenHow>()) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // An O_PATH handle can't fchmod; its /proc/self/fd entry can be chmodded.
    let proc = std::ffi::CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(io::Error::other)?;
    let mode = st.st_mode & 0o7777;
    if unsafe { libc::chmod(proc.as_ptr(), mode) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ESTALE) {
        return Err(err);
    }
    // Removed on the Mac behind a cached entry (berth's own share answers
    // ESTALE). By path, the kernel retries with a fresh lookup and drops
    // what it cached, which is the point of the touch; still no following a
    // symlink at the end (fchmodat2, Linux 6.6+).
    const SYS_FCHMODAT2: libc::c_long = 452;
    if unsafe { libc::syscall(SYS_FCHMODAT2, libc::AT_FDCWD, c.as_ptr(), mode, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read up to and including '\n', one byte at a time so nothing past the
/// header is consumed.
fn read_header(stream: &mut UnixStream) -> io::Result<String> {
    let mut header = Vec::new();
    let mut byte = [0u8];
    while header.len() < MAX_HEADER {
        if stream.read(&mut byte)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if byte[0] == b'\n' {
            return String::from_utf8(header).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        header.push(byte[0]);
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "header too long"))
}

pub(crate) trait Duplex: Read + Write + AsRawFd + Send + 'static {
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>>;
    fn shutdown_write(&self);
}

impl Duplex for UnixStream {
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl Duplex for TcpStream {
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

/// Copy both directions until each side is done, propagating half-closes.
pub(crate) fn splice(a: impl Duplex, b: impl Duplex) -> io::Result<()> {
    let (mut a_read, mut b_write) = (a.try_clone_box()?, b.try_clone_box()?);
    let upstream = thread::spawn(move || {
        let _ = pump(&mut *a_read, &mut *b_write);
        b_write.shutdown_write();
    });
    let (mut b_read, mut a_write) = (b, a);
    let _ = pump(&mut b_read, &mut a_write);
    a_write.shutdown_write();
    let _ = upstream.join();
    Ok(())
}

const PIPE_SIZE: usize = 1 << 20;

/// Move bytes from `from` to `to` until EOF. Uses splice(2) through a pipe so
/// the data never enters userspace; sockets that can't splice (vsock) fall
/// back to a large-buffer copy.
fn pump(from: &mut dyn Duplex, to: &mut dyn Duplex) -> io::Result<u64> {
    // vsock has no splice support; checking first matters for the write side,
    // where a failure would strand data already moved into the pipe.
    if is_vsock(from.as_raw_fd()) || is_vsock(to.as_raw_fd()) {
        return copy_buffered(from, to);
    }
    match splice_all(from.as_raw_fd(), to.as_raw_fd()) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => copy_buffered(from, to),
        other => other,
    }
}

fn is_vsock(fd: RawFd) -> bool {
    let mut domain: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    let rc = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_DOMAIN, (&raw mut domain).cast(), &mut len) };
    rc == 0 && domain == libc::AF_VSOCK
}

fn splice_all(from: RawFd, to: RawFd) -> io::Result<u64> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let (pr, pw) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    unsafe { libc::fcntl(pw.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_SIZE as libc::c_int) };
    let flags = libc::SPLICE_F_MOVE;
    let mut total = 0u64;
    loop {
        let n = unsafe { libc::splice(from, std::ptr::null_mut(), pw.as_raw_fd(), std::ptr::null_mut(), PIPE_SIZE, flags) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // Nothing moved yet: let the caller fall back to copying.
            return Err(err);
        }
        if n == 0 {
            return Ok(total);
        }
        let mut left = n as usize;
        while left > 0 {
            let m = unsafe { libc::splice(pr.as_raw_fd(), std::ptr::null_mut(), to, std::ptr::null_mut(), left, flags) };
            if m < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Data is stuck in the pipe; this connection is done either way.
                // Not EINVAL-as-is, so the caller won't fall back mid-stream.
                return Err(io::Error::other(err));
            }
            left -= m as usize;
        }
        total += n as u64;
    }
}

fn copy_buffered(from: &mut dyn Duplex, to: &mut dyn Duplex) -> io::Result<u64> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        total += n as u64;
    }
}

fn vsock_listen(port: u32) -> io::Result<OwnedFd> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
        if libc::bind(raw, (&raw const addr).cast(), size_of::<libc::sockaddr_vm>() as libc::socklen_t) < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::listen(raw, 128) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

/// Linux vsock lets the peer send only as much as the receive buffer
/// (256 KB by default) before waiting for credit: too little for multi-Gbit
/// transfers from the Mac.
pub(crate) fn widen_vsock_buffer(fd: RawFd) {
    const SO_VM_SOCKETS_BUFFER_SIZE: libc::c_int = 0;
    const SO_VM_SOCKETS_BUFFER_MAX_SIZE: libc::c_int = 2;
    let size: u64 = 4 << 20;
    for opt in [SO_VM_SOCKETS_BUFFER_MAX_SIZE, SO_VM_SOCKETS_BUFFER_SIZE] {
        unsafe {
            libc::setsockopt(fd, libc::AF_VSOCK, opt, (&raw const size).cast(), size_of::<u64>() as libc::socklen_t);
        }
    }
}

fn vsock_accept(listener: &OwnedFd) -> io::Result<OwnedFd> {
    let raw = std::os::fd::AsRawFd::as_raw_fd(listener);
    let fd = unsafe { libc::accept4(raw, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    widen_vsock_buffer(fd);
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
