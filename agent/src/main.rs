//! berth guest agent. Listens on a vsock port; each connection starts with one
//! header line naming where to connect inside the guest, then carries raw bytes:
//!
//!   tcp 127.0.0.1:8080\n
//!   unix /run/docker.sock\n
//!   info domains\n          (replies with the domain table as JSON)
//!
//! A failed connect just closes the stream. The agent also runs the container
//! domain controller (domains.rs) and its mDNS responder (mdns.rs).

mod docker;
mod domains;
mod mdns;

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, OnceLock};
use std::thread;

/// Must match `AGENT_VSOCK_PORT` on the host.
const PORT: u32 = 2400;
const MAX_HEADER: usize = 512;

static TABLE: OnceLock<domains::Shared> = OnceLock::new();

fn main() {
    let table = TABLE.get_or_init(|| Arc::new(Default::default()));
    let (t1, t2) = (table.clone(), table.clone());
    thread::spawn(move || domains::run(t1));
    thread::spawn(move || mdns::run(t2));

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
    match header.split_once(' ') {
        Some(("tcp", addr)) => {
            let guest = TcpStream::connect(addr)?;
            guest.set_nodelay(true)?;
            splice(host, guest)
        }
        Some(("unix", path)) => splice(host, UnixStream::connect(path)?),
        Some(("info", "domains")) => {
            let entries = TABLE.get().unwrap().read().unwrap().entries.clone();
            serde_json::to_writer(&mut host, &entries).map_err(io::Error::other)?;
            Ok(())
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad header {header:?}"))),
    }
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

trait Duplex: Read + Write + Send + 'static {
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
fn splice(a: impl Duplex, b: impl Duplex) -> io::Result<()> {
    let (mut a_read, mut b_write) = (a.try_clone_box()?, b.try_clone_box()?);
    let upstream = thread::spawn(move || {
        let _ = io::copy(&mut a_read, &mut b_write);
        b_write.shutdown_write();
    });
    let (mut b_read, mut a_write) = (b, a);
    let _ = io::copy(&mut b_read, &mut a_write);
    a_write.shutdown_write();
    let _ = upstream.join();
    Ok(())
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
