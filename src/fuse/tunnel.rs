//! Connections from the VM to the Mac's loopback, carried as FUSE "stream
//! files" on a second berth virtio-fs device (tag `berth-tunnel`) instead of
//! vsock. Apple's vsock device moves at most ~6 Gbit/s from the Mac into the
//! VM, for all connections together; berth's own device reads at several
//! GB/s.
//!
//! The guest agent (agent/src/tunnel.rs) opens two files per connection
//! in the mounted share: `tcp-<port>-<id>-down` (opening it connects to
//! 127.0.0.1:<port>, or ::1; reads return what the Mac sent, 0 at its end)
//! and then `tcp-<port>-<id>-up` (writes go to the Mac; closing it ends
//! that direction). They open with FOPEN_DIRECT_IO and FOPEN_STREAM: no page
//! cache, no file position. Requests that wait (connect, read) are answered
//! asynchronously, so a blocked read holds no thread.
//!
//! virtio-fs doesn't pass interrupts on, so a READ the guest waits in can't
//! be cancelled by a signal: an idle one is answered EAGAIN after
//! IDLE_READ, and the agent reads again. That keeps the agent killable and
//! frees the request's virtqueue slot now and then.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use super::{MAX_WRITE, Out, Reader, lerr, op};

const FOPEN_DIRECT_IO: u32 = 1 << 0;
const FOPEN_NONSEEKABLE: u32 = 1 << 2;
const FOPEN_STREAM: u32 = 1 << 4;
const ROOT: u64 = 1;
const IDLE_READ: std::time::Duration = std::time::Duration::from_secs(2);
const EAGAIN: i32 = 11;
const EEXIST: i32 = 17;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Way {
    Down,
    Up,
}

struct Conn {
    read: tokio::sync::Mutex<Option<OwnedReadHalf>>,
    write: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
    /// Wakes blocked reads and writes when the device is saved (snapshots).
    cancel: tokio::sync::Notify,
    /// The guest closed its down file.
    closed: AtomicBool,
}

#[derive(Default)]
pub struct Tunnel {
    /// nodeid -> (connection id, port, direction)
    nodes: Mutex<HashMap<u64, (u64, u16, Way)>>,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    /// fh -> (connection id, direction)
    handles: Mutex<HashMap<u64, (u64, Way)>>,
    next_node: AtomicU64,
    next_fh: AtomicU64,
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("berth-tunnel").enable_all().build().unwrap()
    })
}

/// `tcp-<port>-<id>-down|up`
fn parse_name(name: &[u8]) -> Option<(u16, u64, Way)> {
    let name = std::str::from_utf8(name).ok()?;
    let mut parts = name.split('-');
    if parts.next()? != "tcp" {
        return None;
    }
    let port = parts.next()?.parse().ok()?;
    let id = parts.next()?.parse().ok()?;
    let way = match parts.next()? {
        "down" => Way::Down,
        "up" => Way::Up,
        _ => return None,
    };
    parts.next().is_none().then_some((port, id, way))
}

fn reply(unique: u64, result: Result<Vec<u8>, i32>) -> Vec<u8> {
    let (error, body) = match result {
        Ok(body) => (0, body),
        Err(errno) => (-errno, Vec::new()),
    };
    let mut out = Out::default();
    out.u32((16 + body.len()) as u32).i32(error).u64(unique);
    out.0.extend_from_slice(&body);
    out.0
}

/// fuse_attr for the root (a directory) or a stream file.
fn attr(out: &mut Out, nodeid: u64) {
    let mode = if nodeid == ROOT { 0o040755 } else { 0o100600 };
    out.u64(nodeid).u64(0).u64(0).u64(0).u64(0).u64(0).u32(0).u32(0).u32(0);
    out.u32(mode).u32(1).u32(0).u32(0).u32(0).u32(4096).u32(0);
}

impl Tunnel {
    pub fn new() -> Arc<Self> {
        let t = Tunnel::default();
        t.next_node.store(2, Ordering::Relaxed);
        t.next_fh.store(1, Ordering::Relaxed);
        Arc::new(t)
    }

    /// Handle one request; `done` gets the reply (None for FORGET), maybe
    /// later and from another thread.
    pub fn handle(self: &Arc<Self>, request: Vec<u8>, done: impl FnOnce(Option<Vec<u8>>) + Send + 'static) {
        let mut r = Reader::new(&request);
        let (Some(_), Some(opcode), Some(unique), Some(nodeid)) = (r.u32(), r.u32(), r.u64(), r.u64()) else {
            return done(None);
        };
        let _ = (r.u32(), r.u32(), r.u32(), r.u16(), r.u16());
        let now = |result: Result<Vec<u8>, i32>| Some(reply(unique, result));
        match opcode {
            op::INIT => done(now(Self::init(&mut r))),
            op::FORGET => {
                self.nodes.lock().unwrap().remove(&nodeid);
                done(None)
            }
            op::BATCH_FORGET => {
                let count = r.u32().unwrap_or(0);
                let _ = r.u32();
                let mut nodes = self.nodes.lock().unwrap();
                for _ in 0..count {
                    let (Some(node), Some(_)) = (r.u64(), r.u64()) else { break };
                    nodes.remove(&node);
                }
                drop(nodes);
                done(None)
            }
            op::INTERRUPT => done(None),
            op::DESTROY | op::FLUSH | op::ACCESS => done(now(Ok(Vec::new()))),
            op::LOOKUP => done(now(self.lookup(nodeid, r.cstr().unwrap_or_default()))),
            op::GETATTR => {
                let mut out = Out::default();
                out.u64(0).u32(0).u32(0);
                attr(&mut out, nodeid);
                done(now(Ok(out.0)))
            }
            op::OPEN => {
                let this = self.clone();
                runtime().spawn(async move { done(Some(reply(unique, this.open(nodeid).await))) });
            }
            op::READ => {
                let (fh, _offset, size) = (r.u64().unwrap_or(0), r.u64(), r.u32().unwrap_or(0));
                let this = self.clone();
                runtime().spawn(async move { done(Some(reply(unique, this.read(fh, size).await))) });
            }
            op::WRITE => {
                let (fh, _offset, size) = (r.u64().unwrap_or(0), r.u64(), r.u32().unwrap_or(0));
                let _ = (r.u32(), r.u64(), r.u32(), r.u32());
                let data = r.rest();
                let data = data[..(size as usize).min(data.len())].to_vec();
                let this = self.clone();
                runtime().spawn(async move { done(Some(reply(unique, this.write(fh, data).await))) });
            }
            op::RELEASE => {
                let fh = r.u64().unwrap_or(0);
                let this = self.clone();
                runtime().spawn(async move {
                    this.release(fh).await;
                    done(Some(reply(unique, Ok(Vec::new()))))
                });
            }
            _ => done(now(Err(lerr::ENOSYS))),
        }
    }

    fn init(r: &mut Reader) -> Result<Vec<u8>, i32> {
        let (major, minor, max_readahead) = (r.u32().ok_or(lerr::EINVAL)?, r.u32().ok_or(lerr::EINVAL)?, r.u32().ok_or(lerr::EINVAL)?);
        if major != 7 {
            return Err(lerr::EINVAL);
        }
        let mut out = Out::default();
        out.u32(7)
            .u32(minor.min(super::KERNEL_MINOR))
            .u32(max_readahead)
            .u32(super::FUSE_BIG_WRITES | super::FUSE_MAX_PAGES)
            .u16(16)
            .u16(12)
            .u32(MAX_WRITE)
            .u32(1)
            .u16((MAX_WRITE / 4096) as u16)
            .u16(0)
            .u32(0)
            .u32(0)
            .zeros(24);
        Ok(out.0)
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<Vec<u8>, i32> {
        let (port, id, way) = parse_name(name).filter(|_| parent == ROOT).ok_or(lerr::ENOENT)?;
        let nodeid = self.next_node.fetch_add(1, Ordering::Relaxed);
        self.nodes.lock().unwrap().insert(nodeid, (id, port, way));
        let mut out = Out::default();
        out.u64(nodeid).u64(0).u64(0).u64(0).u32(0).u32(0);
        attr(&mut out, nodeid);
        Ok(out.0)
    }

    fn open_out(fh: u64) -> Vec<u8> {
        let mut out = Out::default();
        out.u64(fh).u32(FOPEN_DIRECT_IO | FOPEN_NONSEEKABLE | FOPEN_STREAM).i32(0);
        out.0
    }

    async fn open(&self, nodeid: u64) -> Result<Vec<u8>, i32> {
        let (id, port, way) = *self.nodes.lock().unwrap().get(&nodeid).ok_or(lerr::ENOENT)?;
        if way == Way::Down {
            if self.conns.lock().unwrap().contains_key(&id) {
                return Err(EEXIST);
            }
            // Many dev servers bind only one of the loopback addresses.
            let stream = match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => s,
                Err(_) => TcpStream::connect(("::1", port)).await.map_err(|e| super::io_errno(&e))?,
            };
            let _ = stream.set_nodelay(true);
            let (read, write) = stream.into_split();
            let conn = Arc::new(Conn {
                read: tokio::sync::Mutex::new(Some(read)),
                write: tokio::sync::Mutex::new(Some(write)),
                cancel: tokio::sync::Notify::new(),
                closed: AtomicBool::new(false),
            });
            self.conns.lock().unwrap().insert(id, conn);
        } else if !self.conns.lock().unwrap().contains_key(&id) {
            return Err(lerr::ENOENT);
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, (id, way));
        Ok(Self::open_out(fh))
    }

    fn conn(&self, fh: u64, want: Way) -> Result<Arc<Conn>, i32> {
        let (id, way) = *self.handles.lock().unwrap().get(&fh).ok_or(lerr::EBADF)?;
        if way != want {
            return Err(lerr::EBADF);
        }
        self.conns.lock().unwrap().get(&id).cloned().ok_or(lerr::EBADF)
    }

    async fn read(&self, fh: u64, size: u32) -> Result<Vec<u8>, i32> {
        let conn = self.conn(fh, Way::Down)?;
        let mut half = conn.read.lock().await;
        let Some(half) = half.as_mut() else { return Ok(Vec::new()) };
        if conn.closed.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        // Wait for data before allocating: idle connections hold no buffer.
        loop {
            tokio::select! {
                r = half.readable() => r.map_err(|e| super::io_errno(&e))?,
                _ = conn.cancel.notified() => return Err(EAGAIN),
                _ = tokio::time::sleep(IDLE_READ) => return Err(EAGAIN),
            }
            // Take everything that has arrived: each READ is a round trip
            // to the guest, so fill it.
            let mut buf = vec![0u8; size.clamp(1, MAX_WRITE) as usize];
            let (mut n, mut end) = (0, false);
            while n < buf.len() {
                match half.try_read(&mut buf[n..]) {
                    Ok(0) => {
                        end = true;
                        break;
                    }
                    Ok(m) => n += m,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if n == 0 => return Err(super::io_errno(&e)),
                    Err(_) => break,
                }
            }
            // Readable with nothing to read was spurious: wait again.
            if n > 0 || end {
                buf.truncate(n);
                return Ok(buf);
            }
        }
    }

    async fn write(&self, fh: u64, data: Vec<u8>) -> Result<Vec<u8>, i32> {
        let conn = self.conn(fh, Way::Up)?;
        let mut half = conn.write.lock().await;
        let half = half.as_mut().ok_or(lerr::EBADF)?;
        // A short count if the device is saved meanwhile (the guest writes
        // the rest after), EAGAIN if nothing was written.
        let mut written = 0;
        while written < data.len() {
            tokio::select! {
                r = half.write(&data[written..]) => written += r.map_err(|e| super::io_errno(&e))?,
                _ = conn.cancel.notified() => break,
            }
        }
        if written == 0 && !data.is_empty() {
            return Err(EAGAIN);
        }
        let mut out = Out::default();
        out.u32(written as u32).u32(0);
        Ok(out.0)
    }

    async fn release(&self, fh: u64) {
        let Some((id, way)) = self.handles.lock().unwrap().remove(&fh) else { return };
        let Some(conn) = self.conns.lock().unwrap().get(&id).cloned() else { return };
        match way {
            // Closing the up file is the guest's end of its data.
            Way::Up => {
                if let Some(mut w) = conn.write.lock().await.take() {
                    let _ = w.shutdown().await;
                }
            }
            Way::Down => {
                conn.closed.store(true, Ordering::SeqCst);
                conn.cancel.notify_waiters();
                conn.read.lock().await.take();
                // The up file was never opened (its open failed): nothing
                // else will end the connection.
                if !self.handles.lock().unwrap().values().any(|&(i, w)| i == id && w == Way::Up) {
                    conn.write.lock().await.take();
                }
            }
        }
        if conn.read.lock().await.is_none() && conn.write.lock().await.is_none() {
            self.conns.lock().unwrap().remove(&id);
        }
    }

    /// Before the VM is saved (it's paused): answer every waiting read and
    /// write now, so none is left in the saved queues. The guest retries
    /// them; after a restore the connections are gone (EBADF).
    pub fn cancel_all(&self) {
        for conn in self.conns.lock().unwrap().values() {
            conn.cancel.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(parse_name(b"tcp-3000-7-down"), Some((3000, 7, Way::Down)));
        assert_eq!(parse_name(b"tcp-80-1-up"), Some((80, 1, Way::Up)));
        for bad in [&b"tcp-3000-7"[..], b"udp-1-1-up", b"tcp-x-1-up", b"tcp-1-1-up-x", b"tcp-70000-1-up"] {
            assert_eq!(parse_name(bad), None);
        }
    }

    fn request(opcode: u32, nodeid: u64, body: &[u8]) -> Vec<u8> {
        let mut out = Out::default();
        out.u32((40 + body.len()) as u32).u32(opcode).u64(9).u64(nodeid).u32(0).u32(0).u32(0).u16(0).u16(0);
        out.0.extend_from_slice(body);
        out.0
    }

    fn call(t: &Arc<Tunnel>, opcode: u32, nodeid: u64, body: &[u8]) -> (i32, Vec<u8>) {
        let (tx, rx) = std::sync::mpsc::channel();
        t.handle(request(opcode, nodeid, body), move |r| {
            let _ = tx.send(r);
        });
        let r = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap().unwrap();
        (i32::from_le_bytes(r[4..8].try_into().unwrap()), r[16..].to_vec())
    }

    #[test]
    fn a_connection_both_ways() {
        // An echo-ish server on the Mac's loopback: sends a greeting, then
        // echoes until the client is done.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"hello").unwrap();
            let mut got = Vec::new();
            s.read_to_end(&mut got).unwrap();
            s.write_all(&got).unwrap();
        });
        let t = Tunnel::new();
        let node = |name: &str| {
            let mut n = name.as_bytes().to_vec();
            n.push(0);
            let (err, body) = call(&t, op::LOOKUP, ROOT, &n);
            assert_eq!(err, 0);
            u64::from_le_bytes(body[0..8].try_into().unwrap())
        };
        let (down, up) = (node(&format!("tcp-{port}-1-down")), node(&format!("tcp-{port}-1-up")));
        let fh = |nodeid| {
            let (err, body) = call(&t, op::OPEN, nodeid, &[0; 8]);
            assert_eq!(err, 0);
            u64::from_le_bytes(body[0..8].try_into().unwrap())
        };
        let (dfh, ufh) = (fh(down), fh(up));
        let read = |fh: u64| {
            let mut b = Out::default();
            b.u64(fh).u64(0).u32(1 << 20).u32(0).u64(0).u32(0).u32(0);
            call(&t, op::READ, down, &b.0)
        };
        assert_eq!(read(dfh), (0, b"hello".to_vec()));
        let mut w = Out::default();
        w.u64(ufh).u64(0).u32(4).u32(0).u64(0).u32(0).u32(0);
        w.0.extend_from_slice(b"ping");
        assert_eq!(call(&t, op::WRITE, up, &w.0).0, 0);
        assert_eq!(call(&t, op::RELEASE, up, &ufh.to_le_bytes()).0, 0, "ends the guest's side");
        assert_eq!(read(dfh), (0, b"ping".to_vec()));
        assert_eq!(read(dfh), (0, Vec::new()), "EOF when the Mac closes");
        server.join().unwrap();
        let refused = node("tcp-1-2-down");
        assert_eq!(call(&t, op::OPEN, refused, &[0; 8]).0, -111, "ECONNREFUSED: nothing listening");
    }

    #[test]
    fn idle_reads_come_back() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accepts and stays silent.
        let _server = std::thread::spawn(move || {
            let _conn = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(4));
        });
        let t = Tunnel::new();
        let mut n = format!("tcp-{port}-5-down").into_bytes();
        n.push(0);
        let (_, body) = call(&t, op::LOOKUP, ROOT, &n);
        let down = u64::from_le_bytes(body[0..8].try_into().unwrap());
        let (err, body) = call(&t, op::OPEN, down, &[0; 8]);
        assert_eq!(err, 0);
        let fh = u64::from_le_bytes(body[0..8].try_into().unwrap());
        assert_eq!(call(&t, op::OPEN, down, &[0; 8]).0, -EEXIST, "one connection per id");
        let mut b = Out::default();
        b.u64(fh).u64(0).u32(1 << 20).u32(0).u64(0).u32(0).u32(0);
        assert_eq!(call(&t, op::READ, down, &b.0).0, -EAGAIN, "an idle read is answered");
        assert_eq!(call(&t, op::RELEASE, down, &fh.to_le_bytes()).0, 0);
        assert!(t.conns.lock().unwrap().is_empty(), "down released without up ends the connection");
        call_forget(&t, down);
        assert!(t.nodes.lock().unwrap().is_empty(), "FORGET drops the node");
    }

    fn call_forget(t: &Arc<Tunnel>, nodeid: u64) {
        let (tx, rx) = std::sync::mpsc::channel();
        t.handle(request(op::FORGET, nodeid, &1u64.to_le_bytes()), move |r| {
            let _ = tx.send(r);
        });
        assert!(rx.recv().unwrap().is_none());
    }
}
