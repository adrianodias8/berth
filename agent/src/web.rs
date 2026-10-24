//! Web access to container domains without port numbers, and HTTPS.
//!
//! iptables redirects ports 80 and 443 of every service IP here. For HTTP we
//! find the container's web port and splice to it. For HTTPS we hand the
//! connection to the host daemon, which terminates TLS with a certificate from
//! berth's local CA and asks us for a plain connection to that same port;
//! unless the container serves 443 itself, in which case it is passed through.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

use crate::domains::{Service, Shared};
use crate::hostnet::{HOST_VSOCK_PORT, VMADDR_CID_HOST, original_dst, vsock_connect};

pub const HTTP_PORT: u16 = 2403;
pub const HTTPS_PORT: u16 = 2404;
/// Override detection with a label; OrbStack's spelling works too.
const PORT_LABELS: [&str; 2] = ["dev.berth.http-port", "dev.orbstack.http-port"];
/// Tried first, in order, among the ports a container listens on.
const PREFERRED: [u16; 10] = [80, 8080, 3000, 8000, 5000, 5173, 4000, 8888, 9000, 4200];

/// Detected web port per container, keyed by id and valid for one process
/// lifetime of the container (its init PID).
static CACHE: Mutex<Option<HashMap<String, (u32, u16)>>> = Mutex::new(None);

pub fn run(table: Shared) {
    for (port, https) in [(HTTP_PORT, false), (HTTPS_PORT, true)] {
        let table = table.clone();
        std::thread::spawn(move || serve(port, https, table));
    }
}

fn serve(port: u16, https: bool, table: Shared) {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("berth-agent: web: listen {port}: {err}");
            return;
        }
    };
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let table = table.clone();
        std::thread::spawn(move || {
            if let Err(err) = handle(conn, https, &table) {
                eprintln!("berth-agent: web: {err}");
            }
        });
    }
}

fn handle(mut conn: TcpStream, https: bool, table: &Shared) -> std::io::Result<()> {
    // Only connections redirected for a service IP; anything else that
    // reaches this port directly is dropped.
    let dst = original_dst(&conn)?;
    let Some(service) = table.read().unwrap().services.get(dst.ip()).cloned() else { return Ok(()) };
    conn.set_nodelay(true)?;

    // Linux machines aren't Docker containers: no detection or TLS, just the
    // port that was asked for.
    if service.id.starts_with("machine:") {
        return crate::splice(conn, TcpStream::connect((service.ip, dst.port()))?);
    }

    if https {
        if listening_ports(&service).contains(&443) {
            let upstream = TcpStream::connect((service.ip, 443))?;
            return crate::splice(conn, upstream);
        }
        let port = web_port(&service).unwrap_or(0);
        let mut host = std::os::unix::net::UnixStream::from(vsock_connect(VMADDR_CID_HOST, HOST_VSOCK_PORT)?);
        writeln!(host, "https {} {port}", service.ip)?;
        return crate::splice(conn, host);
    }

    match web_port(&service) {
        Some(port) => crate::splice(conn, TcpStream::connect((service.ip, port))?),
        None => {
            let body = format!("berth: no web server found in container {}\n", service.name);
            write!(
                conn,
                "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }
    }
}

/// The container's HTTP port: the label if set, else the first listening port
/// (preferred ones first) that answers an HTTP request.
pub fn web_port(service: &Service) -> Option<u16> {
    let info = crate::docker::inspect(&service.id).ok()?;
    let labels = &info["Config"]["Labels"];
    if let Some(port) = PORT_LABELS.iter().find_map(|l| labels[l].as_str()?.trim().parse().ok()) {
        return Some(port);
    }
    let pid = info["State"]["Pid"].as_u64()? as u32;
    if let Some(&(cached_pid, port)) = CACHE.lock().unwrap().as_ref().and_then(|c| c.get(&service.id))
        && cached_pid == pid
    {
        return Some(port);
    }
    // A container with no web server: don't probe its ports on every request.
    if MISSES.lock().unwrap().get(&service.id).is_some_and(|&(p, at)| p == pid && at.elapsed() < MISS_TTL) {
        return None;
    }
    let Some(port) = order(listening_ports_of(pid)).into_iter().find(|&p| speaks_http(service.ip, p)) else {
        MISSES.lock().unwrap().insert(service.id.clone(), (pid, std::time::Instant::now()));
        return None;
    };
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(service.id.clone(), (pid, port));
    Some(port)
}

/// Containers found without a web port, and when (re-probed after MISS_TTL:
/// a server may still be starting).
static MISSES: std::sync::LazyLock<Mutex<HashMap<String, (u32, std::time::Instant)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
const MISS_TTL: std::time::Duration = std::time::Duration::from_secs(10);

fn order(mut ports: Vec<u16>) -> Vec<u16> {
    ports.sort_by_key(|p| (PREFERRED.iter().position(|q| q == p).unwrap_or(usize::MAX), *p));
    ports.dedup();
    ports
}

fn listening_ports(service: &Service) -> Vec<u16> {
    crate::docker::inspect(&service.id)
        .ok()
        .and_then(|info| info["State"]["Pid"].as_u64())
        .map_or_else(Vec::new, |pid| listening_ports_of(pid as u32))
}

/// TCP ports listening on a non-loopback address in the container's network
/// namespace, read from its /proc/<pid>/net tables.
fn listening_ports_of(pid: u32) -> Vec<u16> {
    let mut ports = Vec::new();
    for table in ["tcp", "tcp6"] {
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) else { continue };
        ports.extend(parse_listeners(&text));
    }
    ports
}

fn parse_listeners(table: &str) -> Vec<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (local, state) = (fields.get(1)?, fields.get(3)?);
            if *state != "0A" {
                return None; // not LISTEN
            }
            let (addr, port) = local.split_once(':')?;
            // Loopback-only listeners can't be reached via the container IP.
            // Addresses are host-order hex words: 127.x.x.x ends in "7F",
            // ::1 is "...01000000", ::ffff:127.x ends in "7F" too.
            if addr.ends_with("7F") || addr == "00000000000000000000000001000000" {
                return None;
            }
            u16::from_str_radix(port, 16).ok()
        })
        .collect()
}

fn speaks_http(ip: Ipv4Addr, port: u16) -> bool {
    let probe = || -> std::io::Result<bool> {
        let mut s = TcpStream::connect_timeout(&SocketAddr::from((ip, port)), Duration::from_millis(300))?;
        s.set_read_timeout(Some(Duration::from_millis(1500)))?;
        s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\nUser-Agent: berth-port-detection\r\nConnection: close\r\n\r\n")?;
        let mut head = [0u8; 5];
        s.read_exact(&mut head)?;
        Ok(&head == b"HTTP/")
    };
    probe().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listeners_skip_loopback_and_non_listen() {
        let tcp = "  sl  local_address rem_address   st\n\
                   0: 00000000:0050 00000000:0000 0A\n\
                   1: 0100007F:1F90 00000000:0000 0A\n\
                   2: 020011AC:0BB8 00000000:0000 0A\n\
                   3: 020011AC:0050 050011AC:D2F0 01\n";
        assert_eq!(parse_listeners(tcp), vec![80, 3000]);
        let tcp6 = "  sl  local_address rem_address st\n\
                    0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A\n\
                    1: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A\n";
        assert_eq!(parse_listeners(tcp6), vec![8080]);
    }

    #[test]
    fn preferred_ports_first() {
        assert_eq!(order(vec![9229, 3000, 22, 8080]), vec![8080, 3000, 22, 9229]);
    }
}
