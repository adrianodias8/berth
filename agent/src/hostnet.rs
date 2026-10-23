//! host.docker.internal: the Mac as seen from containers.
//!
//! `HOST_IP` lives on the guest's loopback. iptables redirects TCP for it to
//! `REDIRECT_PORT`, where we recover the original port and hand the connection
//! to the host daemon over vsock, which connects to that port on the Mac's
//! loopback. Names come from the DNS server in dns.rs, which Docker is
//! configured to use.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::Command;

pub const HOST_IP: Ipv4Addr = Ipv4Addr::new(198, 19, 248, 254);
pub const DNS_IP: Ipv4Addr = Ipv4Addr::new(198, 19, 248, 253);
/// Names that resolve to `HOST_IP` inside containers.
pub const HOST_NAMES: [&str; 5] = [
    "host.docker.internal",
    "gateway.docker.internal",
    "host.berth.internal",
    "docker.for.mac.host.internal",
    "docker.for.mac.localhost",
];
const REDIRECT_PORT: u16 = 2401;
/// Must match `HOST_VSOCK_PORT` on the host.
pub(crate) const HOST_VSOCK_PORT: u32 = 2401;
pub(crate) const VMADDR_CID_HOST: u32 = 2;
const SO_ORIGINAL_DST: libc::c_int = 80;
const DOCKER_CONFIG: &str = "/etc/docker/daemon.json";

/// Addresses, NAT rules and Docker's DNS setting. Idempotent; run at startup.
pub fn setup() {
    for ip in [HOST_IP, DNS_IP] {
        let addr = format!("{ip}/32");
        if !quiet("ip", &["-4", "addr", "show", "dev", "lo", "to", &addr]).is_some_and(|o| !o.is_empty()) {
            quiet("ip", &["addr", "add", &addr, "dev", "lo"]);
        }
    }
    // PREROUTING for containers, OUTPUT for the guest itself.
    let port = REDIRECT_PORT.to_string();
    for chain in ["PREROUTING", "OUTPUT"] {
        let rule = ["-t", "nat", "-p", "tcp", "-d", "198.19.248.254/32", "-j", "REDIRECT", "--to-ports", &port];
        let check: Vec<&str> = [&["-C", chain][..], &rule].concat();
        if quiet("iptables", &check).is_none() {
            let insert: Vec<&str> = [&["-I", chain, "1"][..], &rule].concat();
            if quiet("iptables", &insert).is_none() {
                eprintln!("berth-agent: hostnet: adding {chain} redirect failed");
            }
        }
    }
    if let Err(err) = configure_docker() {
        eprintln!("berth-agent: hostnet: docker config: {err}");
    }
}

/// Our daemon.json settings, keeping any others. Restarts dockerd only when
/// the file actually changes. Also called by engine.rs once docker-ce is in.
///
/// - dns: our DNS server (host.docker.internal, *.berth.local).
/// - allow-direct-routing: Docker 28+ drops packets addressed to container
///   IPs from outside the bridge (raw PREROUTING); the Mac routes container
///   subnets to us on purpose. Only written for docker-ce: docker.io 26
///   refuses to start on unknown keys.
pub fn configure_docker() -> io::Result<()> {
    let mut config: serde_json::Map<String, serde_json::Value> = match std::fs::read(DOCKER_CONFIG) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e),
    };
    let before = config.clone();
    config.insert("dns".into(), serde_json::json!([DNS_IP.to_string()]));
    if crate::engine::installed("docker-ce") {
        config.insert("allow-direct-routing".into(), serde_json::Value::Bool(true));
    }
    if config == before {
        return Ok(());
    }
    std::fs::create_dir_all("/etc/docker")?;
    let tmp = format!("{DOCKER_CONFIG}.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&config).map_err(io::Error::other)?)?;
    std::fs::rename(&tmp, DOCKER_CONFIG)?;
    eprintln!("berth-agent: hostnet: updated {DOCKER_CONFIG}, restarting dockerd");
    quiet("systemctl", &["restart", "docker"]);
    Ok(())
}

pub fn run() {
    let listener = match TcpListener::bind(("0.0.0.0", REDIRECT_PORT)) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("berth-agent: hostnet: listen {REDIRECT_PORT}: {err}");
            return;
        }
    };
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        std::thread::spawn(move || {
            if let Err(err) = forward(conn) {
                eprintln!("berth-agent: hostnet: {err}");
            }
        });
    }
}

fn forward(conn: TcpStream) -> io::Result<()> {
    // Only redirected connections for HOST_IP; anything reaching this port
    // directly (e.g. from the NAT network) has no business here.
    let dst = original_dst(&conn)?;
    if *dst.ip() != HOST_IP {
        return Ok(());
    }
    conn.set_nodelay(true)?;
    let mut host = UnixStream::from(vsock_connect(VMADDR_CID_HOST, HOST_VSOCK_PORT)?);
    writeln!(host, "tcp {}", dst.port())?;
    crate::splice(conn, host)
}

pub(crate) fn original_dst(conn: &TcpStream) -> io::Result<SocketAddrV4> {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(conn.as_raw_fd(), libc::SOL_IP, SO_ORIGINAL_DST, (&raw mut addr).cast(), &mut len)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SocketAddrV4::new(Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)), u16::from_be(addr.sin_port)))
}

pub(crate) fn vsock_connect(cid: u32, port: u32) -> io::Result<OwnedFd> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = cid;
        if libc::connect(fd.as_raw_fd(), (&raw const addr).cast(), size_of::<libc::sockaddr_vm>() as libc::socklen_t) < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn quiet(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
