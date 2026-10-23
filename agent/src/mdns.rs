//! Multicast DNS responder for the domain table (RFC 6762). macOS sends every
//! `.local` lookup over mDNS on all interfaces, including the one facing this
//! VM, so answering here needs no resolver configuration on the Mac.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};

use crate::domains::Shared;

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_NSEC: u16 = 47;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const CACHE_FLUSH: u16 = 0x8000;
const UNICAST_RESPONSE: u16 = 0x8000;
const TTL: u32 = 60;
/// Legacy unicast queries (not from port 5353) get short TTLs, per §6.7.
const LEGACY_TTL: u32 = 10;

pub fn run(table: Shared) {
    loop {
        if let Err(err) = serve(&table) {
            eprintln!("berth-agent: mdns: {err}");
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn serve(table: &Shared) -> io::Result<()> {
    let socket = bind()?;
    let mut joined = None;
    let mut buf = [0u8; 9000];
    loop {
        // (Re)join on the uplink once its address is known or if it changed.
        let iface = table.read().unwrap().guest_ip;
        if let Some(ip) = iface
            && iface != joined
        {
            if let Some(old) = joined {
                let _ = socket.leave_multicast_v4(&GROUP, &old);
            }
            socket.join_multicast_v4(&GROUP, &ip)?;
            set_multicast_if(&socket, ip)?;
            joined = Some(ip);
        }

        let (len, src) = match socket.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        };
        let Some(query) = Query::parse(&buf[..len]) else { continue };
        let legacy = src.port() != PORT;
        let table = table.read().unwrap();
        let mut answers = Vec::new();
        for q in &query.questions {
            if q.qclass & !UNICAST_RESPONSE != CLASS_IN && q.qclass & !UNICAST_RESPONSE != TYPE_ANY {
                continue;
            }
            if let Some(ip) = table.lookup(&q.name) {
                answers.push((q, ip));
            }
        }
        drop(table);
        if answers.is_empty() {
            continue;
        }
        let response = build_response(&query, &answers, legacy);
        let unicast = legacy || answers.iter().all(|(q, _)| q.qclass & UNICAST_RESPONSE != 0);
        let dest = if unicast { src } else { SocketAddr::V4(SocketAddrV4::new(GROUP, PORT)) };
        let _ = socket.send_to(&response, dest);
    }
}

/// Tell caches on the Mac that these names are gone (TTL 0, RFC 6762 §10.1)
/// instead of letting them linger for a full TTL.
pub fn goodbye(names: &[(String, Ipv4Addr)], iface: Ipv4Addr) -> io::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let socket = bind()?;
    set_multicast_if(&socket, iface)?;
    let mut records = Vec::new();
    for (name, ip) in names {
        write_record(&mut records, name, TYPE_A, 0, &ip.octets(), true);
    }
    let mut out = vec![0, 0, 0x84, 0];
    for count in [0u16, names.len() as u16, 0, 0] {
        out.extend_from_slice(&count.to_be_bytes());
    }
    out.extend_from_slice(&records);
    socket.send_to(&out, SocketAddrV4::new(GROUP, PORT))?;
    Ok(())
}

fn bind() -> io::Result<UdpSocket> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = UdpSocket::from_raw_fd(fd);
        let one: libc::c_int = 1;
        for opt in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
            libc::setsockopt(fd, libc::SOL_SOCKET, opt, (&raw const one).cast(), size_of::<libc::c_int>() as u32);
        }
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: PORT.to_be(),
            sin_addr: libc::in_addr { s_addr: 0 },
            sin_zero: [0; 8],
        };
        if libc::bind(fd, (&raw const addr).cast(), size_of::<libc::sockaddr_in>() as u32) < 0 {
            return Err(io::Error::last_os_error());
        }
        socket.set_multicast_ttl_v4(255)?;
        socket.set_multicast_loop_v4(false)?;
        // Wake up periodically to notice uplink address changes.
        socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        Ok(socket)
    }
}

fn set_multicast_if(socket: &UdpSocket, ip: Ipv4Addr) -> io::Result<()> {
    let addr = libc::in_addr { s_addr: u32::from(ip).to_be() };
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&raw const addr).cast(),
            size_of::<libc::in_addr>() as u32,
        )
    };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

struct Question {
    name: String,
    qtype: u16,
    qclass: u16,
}

struct Query {
    id: u16,
    questions: Vec<Question>,
}

impl Query {
    fn parse(msg: &[u8]) -> Option<Self> {
        if msg.len() < 12 {
            return None;
        }
        let id = u16::from_be_bytes([msg[0], msg[1]]);
        let flags = u16::from_be_bytes([msg[2], msg[3]]);
        if flags & 0x8000 != 0 {
            return None; // a response, not a query
        }
        let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
        let mut pos = 12;
        let mut questions = Vec::new();
        for _ in 0..qdcount {
            let (name, next) = read_name(msg, pos)?;
            let fixed = msg.get(next..next + 4)?;
            questions.push(Question {
                name,
                qtype: u16::from_be_bytes([fixed[0], fixed[1]]),
                qclass: u16::from_be_bytes([fixed[2], fixed[3]]),
            });
            pos = next + 4;
        }
        Some(Query { id, questions })
    }
}

/// Read a possibly compressed name; returns it and the offset after it.
pub(crate) fn read_name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    for _ in 0..128 {
        let len = *msg.get(pos)? as usize;
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(pos + 1)));
        }
        if len & 0xc0 == 0xc0 {
            let ptr = (len & 0x3f) << 8 | *msg.get(pos + 1)? as usize;
            end.get_or_insert(pos + 2);
            pos = ptr;
            continue;
        }
        labels.push(String::from_utf8_lossy(msg.get(pos + 1..pos + 1 + len)?).into_owned());
        pos += 1 + len;
    }
    None
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn write_record(out: &mut Vec<u8>, name: &str, rtype: u16, ttl: u32, rdata: &[u8], flush: bool) {
    write_name(out, name);
    out.extend_from_slice(&rtype.to_be_bytes());
    out.extend_from_slice(&(CLASS_IN | if flush { CACHE_FLUSH } else { 0 }).to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
}

/// A records for A/ANY questions; for AAAA questions an NSEC record saying the
/// name has only an A record, so the Mac does not wait for an IPv6 answer.
fn build_response(query: &Query, answers: &[(&Question, Ipv4Addr)], legacy: bool) -> Vec<u8> {
    let ttl = if legacy { LEGACY_TTL } else { TTL };
    let flush = !legacy;
    let mut answer = Vec::new();
    let mut additional = Vec::new();
    let (mut an, mut ar) = (0u16, 0u16);
    for (q, ip) in answers {
        let mut nsec = Vec::new();
        write_name(&mut nsec, &q.name);
        nsec.extend_from_slice(&[0, 1, 0x40]); // window 0, 1 byte, type 1 (A)
        match q.qtype {
            TYPE_A | TYPE_ANY => {
                write_record(&mut answer, &q.name, TYPE_A, ttl, &ip.octets(), flush);
                an += 1;
                write_record(&mut additional, &q.name, TYPE_NSEC, ttl, &nsec, flush);
                ar += 1;
            }
            TYPE_AAAA => {
                write_record(&mut answer, &q.name, TYPE_NSEC, ttl, &nsec, flush);
                an += 1;
                write_record(&mut additional, &q.name, TYPE_A, ttl, &ip.octets(), flush);
                ar += 1;
            }
            _ => {}
        }
    }

    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(&(if legacy { query.id } else { 0 }).to_be_bytes());
    out.extend_from_slice(&0x8400u16.to_be_bytes()); // response, authoritative
    let qd = if legacy { answers.len() as u16 } else { 0 };
    for count in [qd, an, 0, ar] {
        out.extend_from_slice(&count.to_be_bytes());
    }
    if legacy {
        // Legacy unicast responses must echo the questions.
        for (q, _) in answers {
            write_name(&mut out, &q.name);
            out.extend_from_slice(&q.qtype.to_be_bytes());
            out.extend_from_slice(&CLASS_IN.to_be_bytes());
        }
    }
    out.extend_from_slice(&answer);
    out.extend_from_slice(&additional);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut msg = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        write_name(&mut msg, name);
        msg.extend_from_slice(&qtype.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());
        msg
    }

    #[test]
    fn parses_query() {
        let q = Query::parse(&query("web.berth.local", TYPE_A)).unwrap();
        assert_eq!(q.questions.len(), 1);
        assert_eq!(q.questions[0].name, "web.berth.local");
        assert_eq!(q.questions[0].qtype, TYPE_A);
    }

    #[test]
    fn follows_compression_pointer() {
        // "a" then a pointer to offset 12 ("web.berth.local").
        let mut msg = query("web.berth.local", TYPE_A);
        let ptr_target = 12u16;
        msg[5] = 2;
        msg.extend_from_slice(&[1, b'a', 0xc0 | (ptr_target >> 8) as u8, ptr_target as u8]);
        msg.extend_from_slice(&TYPE_A.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());
        let q = Query::parse(&msg).unwrap();
        assert_eq!(q.questions[1].name, "a.web.berth.local");
    }

    #[test]
    fn a_response_layout() {
        let q = Query::parse(&query("web.berth.local", TYPE_A)).unwrap();
        let ip = Ipv4Addr::new(192, 168, 64, 200);
        let resp = build_response(&q, &[(&q.questions[0], ip)], false);
        assert_eq!(&resp[2..4], &[0x84, 0x00]);
        assert_eq!(&resp[4..12], &[0, 0, 0, 1, 0, 0, 0, 1]);
        let name_end = 12 + "web.berth.local".len() + 2;
        assert_eq!(&resp[name_end..name_end + 4], &[0, 1, 0x80, 1]);
        assert_eq!(&resp[name_end + 10..name_end + 14], &ip.octets());
    }
}
