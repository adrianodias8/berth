//! berth guest agent. Listens on a vsock port; each connection starts with one
//! header line naming where to connect inside the guest, then carries raw bytes:
//!
//!   tcp 127.0.0.1:8080\n
//!   unix /run/docker.sock\n
//!   info domains\n          (replies with the domain table as JSON)
//!   info network\n          (replies with the guest address and Docker subnets)
//!   info memory\n           (replies with MemTotal/MemAvailable/PSI, see memory.rs)
//!   touch\n                 followed by newline-separated paths until EOF: raise
//!                           inotify events for files changed on the Mac
//!
//! A failed connect just closes the stream. The agent also runs the container
//! domain controller (domains.rs), its mDNS responder (mdns.rs), DNS for
//! containers (dns.rs), host.docker.internal forwarding (hostnet.rs) and web
//! access to domains: port detection and HTTPS (web.rs).

mod boot;
mod dns;
mod docker;
mod domains;
mod engine;
mod hostnet;
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
    thread::spawn(move || domains::run(t1));
    thread::spawn(move || mdns::run(t2));
    thread::spawn(move || dns::run(t3));
    thread::spawn(hostnet::run);

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
    match header.split_once(' ') {
        Some(("tcp", addr)) => {
            let guest = TcpStream::connect(addr)?;
            guest.set_nodelay(true)?;
            splice(host, guest)
        }
        Some(("unix", path)) => splice(host, UnixStream::connect(path)?),
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

/// chmod each path to its current mode: a no-op that still raises IN_ATTRIB
/// for inotify watchers in the guest and refreshes virtiofs's cached
/// attributes. Missing paths (deleted on the Mac) are skipped; the host also
/// sends their parent directory.
fn touch_paths(mut host: UnixStream) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut list = String::new();
    host.read_to_string(&mut list)?;
    for path in list.lines().filter(|l| !l.is_empty()) {
        if let Ok(meta) = std::fs::symlink_metadata(path)
            && !meta.file_type().is_symlink()
        {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(meta.permissions().mode()));
        }
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

fn vsock_accept(listener: &OwnedFd) -> io::Result<OwnedFd> {
    let raw = std::os::fd::AsRawFd::as_raw_fd(listener);
    let fd = unsafe { libc::accept4(raw, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
