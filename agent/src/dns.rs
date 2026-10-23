//! DNS for containers, on `DNS_IP:53`. Docker hands this address to every
//! container (and its embedded resolver forwards to it), so we can answer
//! host.docker.internal and `*.berth.local` names; everything else is relayed
//! to the guest's upstream resolver, which is the Mac's, so VPN and split DNS
//! settings carry over.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use crate::domains::{SUFFIX, Shared};
use crate::hostnet::{DNS_IP, HOST_IP, HOST_NAMES};
use crate::mdns::read_name;

const TYPE_A: u16 = 1;
const RCODE_NXDOMAIN: u16 = 3;
const RCODE_SERVFAIL: u16 = 2;
const TTL: u32 = 30;

pub fn run(table: Shared) {
    let socket = loop {
        // DNS_IP is added by hostnet::setup; wait for it if we raced.
        match UdpSocket::bind((DNS_IP, 53)) {
            Ok(s) => break s,
            Err(err) => {
                eprintln!("berth-agent: dns: bind: {err}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    let mut buf = [0u8; 4096];
    loop {
        let Ok((len, src)) = socket.recv_from(&mut buf) else { continue };
        let query = buf[..len].to_vec();
        match answer_locally(&query, &table) {
            Some(reply) => {
                let _ = socket.send_to(&reply, src);
            }
            None => {
                let socket = socket.try_clone().expect("clone dns socket");
                std::thread::spawn(move || {
                    let reply = relay(&query).unwrap_or_else(|_| error_reply(&query, RCODE_SERVFAIL));
                    let _ = socket.send_to(&reply, src);
                });
            }
        }
    }
}

/// Our names: A answers, empty NOERROR for other types, NXDOMAIN for unknown
/// names under our suffix. None means "not ours, relay it".
fn answer_locally(query: &[u8], table: &Shared) -> Option<Vec<u8>> {
    if query.len() < 12 || u16::from_be_bytes([query[4], query[5]]) != 1 {
        return None;
    }
    let (name, end) = read_name(query, 12)?;
    let qtype = u16::from_be_bytes([*query.get(end)?, *query.get(end + 1)?]);
    let name = name.to_ascii_lowercase();
    let ip = if HOST_NAMES.contains(&name.as_str()) {
        Some(HOST_IP)
    } else if name == SUFFIX || name.ends_with(&format!(".{SUFFIX}")) || ends_with_custom(&name, table) {
        table.read().unwrap().lookup(&name)
    } else {
        return None;
    };
    let Some(ip) = ip else {
        return Some(error_reply(query, RCODE_NXDOMAIN));
    };
    Some(reply(query, end + 4, (qtype == TYPE_A).then_some(ip)))
}

/// Custom label domains (e.g. `foo.local`) are ours only if some container
/// claims them; other `.local` names go upstream.
fn ends_with_custom(name: &str, table: &Shared) -> bool {
    name.ends_with(".local") && table.read().unwrap().lookup(name).is_some()
}

fn reply(query: &[u8], question_end: usize, ip: Option<Ipv4Addr>) -> Vec<u8> {
    let mut out = Vec::with_capacity(question_end + 16);
    out.extend_from_slice(&query[..2]); // id
    let rd = query[2] & 0x01;
    out.extend_from_slice(&[0x84 | rd, 0x80]); // QR, AA, RD copied, RA
    out.extend_from_slice(&[0, 1, 0, u8::from(ip.is_some()), 0, 0, 0, 0]);
    out.extend_from_slice(&query[12..question_end]);
    if let Some(ip) = ip {
        out.extend_from_slice(&[0xc0, 12]); // pointer to the question name
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&TTL.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&ip.octets());
    }
    out
}

fn error_reply(query: &[u8], rcode: u16) -> Vec<u8> {
    let mut out = query.to_vec();
    if out.len() >= 4 {
        out[2] = 0x80 | (out[2] & 0x01);
        out[3] = 0x80 | rcode as u8;
        // Keep the question, drop any answer/authority/additional counts.
        out[6..12].fill(0);
        let end = read_name(query, 12).map_or(query.len(), |(_, e)| (e + 4).min(query.len()));
        out.truncate(end);
    }
    out
}

fn upstream() -> Option<SocketAddr> {
    let conf = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    conf.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|ip| ip.trim().parse::<std::net::IpAddr>().ok())
        .find(|ip| *ip != std::net::IpAddr::V4(DNS_IP))
        .map(|ip| SocketAddr::new(ip, 53))
}

fn relay(query: &[u8]) -> io::Result<Vec<u8>> {
    let server = upstream().ok_or_else(|| io::Error::other("no upstream nameserver"))?;
    let socket = UdpSocket::bind(if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    socket.set_read_timeout(Some(Duration::from_secs(4)))?;
    socket.send_to(query, server)?;
    let mut buf = [0u8; 4096];
    loop {
        let (len, from) = socket.recv_from(&mut buf)?;
        // Ignore stray packets; the reply must match the query id.
        if from == server && len >= 2 && buf[..2] == query[..2] {
            return Ok(buf[..len].to_vec());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    fn table() -> Shared {
        Arc::new(RwLock::new(Default::default()))
    }

    #[test]
    fn host_docker_internal() {
        let r = answer_locally(&query("host.docker.internal", TYPE_A), &table()).unwrap();
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[3] & 0x0f, 0, "NOERROR");
        assert_eq!(&r[6..8], &[0, 1], "one answer");
        assert_eq!(&r[r.len() - 4..], &HOST_IP.octets());
    }

    #[test]
    fn aaaa_is_empty_noerror() {
        let r = answer_locally(&query("host.docker.internal", 28), &table()).unwrap();
        assert_eq!(r[3] & 0x0f, 0);
        assert_eq!(&r[6..8], &[0, 0]);
    }

    #[test]
    fn unknown_berth_name_is_nxdomain() {
        let r = answer_locally(&query("nope.berth.local", TYPE_A), &table()).unwrap();
        assert_eq!(r[3] & 0x0f, 3);
    }

    #[test]
    fn other_names_are_relayed() {
        assert!(answer_locally(&query("example.com", TYPE_A), &table()).is_none());
        assert!(answer_locally(&query("printer.local", TYPE_A), &table()).is_none());
    }
}
