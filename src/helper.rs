//! Privileged route helper. Container subnets live inside the VM; for the Mac
//! to reach container IPs directly it needs a route per subnet via the guest,
//! and routes need root. This runs as a LaunchDaemon installed once with
//! `sudo berth helper install`. It keeps a set of routes for private subnets
//! via a VM on a vmnet bridge, points /var/run/docker.sock at the user's
//! berth socket, and binds privileged loopback ports for port forwarding
//! (macOS lets users bind ports below 1024 only on all interfaces), for one
//! user.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const SOCKET: &str = "/var/run/dev.berth.helper.sock";
const LABEL: &str = "dev.berth.helper";
const BINARY: &str = "/Library/PrivilegedHelperTools/dev.berth.helper";
const PLIST: &str = "/Library/LaunchDaemons/dev.berth.helper.plist";

/// Private ranges a container subnet may come from.
const ALLOWED: [(Ipv4Addr, u8); 5] = [
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
];

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Route {
    pub subnet: String,
    pub gateway: String,
}

/// Bumped when the helper gains abilities, so berth can ask for a reinstall.
pub const VERSION: u32 = 4;

#[derive(Serialize, Deserialize)]
pub struct SetRoutes {
    pub routes: Vec<Route>,
    /// The user's ~/.berth/bin: link docker/docker-compose in /usr/local/bin
    /// to it where those are missing or dangling.
    #[serde(default)]
    pub cli_dir: Option<String>,
    /// Point /var/run/docker.sock here. Must be the user's
    /// ~/.berth/run/docker.sock; None leaves the link alone.
    #[serde(default)]
    pub docker_socket: Option<String>,
}

/// The system-wide default Docker socket that context-unaware tools use.
const DEFAULT_DOCKER_SOCKET: &str = "/var/run/docker.sock";

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Reply {
    pub active: Vec<Route>,
    pub errors: Vec<String>,
    /// 0 from helpers that predate versioning.
    #[serde(default)]
    pub version: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Cidr {
    net: u32,
    len: u8,
}

impl Cidr {
    fn parse(s: &str) -> Option<Self> {
        let (ip, len) = s.split_once('/')?;
        let ip: Ipv4Addr = ip.parse().ok()?;
        let len: u8 = len.parse().ok()?;
        if len > 32 {
            return None;
        }
        let net = u32::from(ip);
        (net & !mask(len) == 0).then_some(Self { net, len })
    }

    fn overlaps(self, other: Cidr) -> bool {
        let len = self.len.min(other.len);
        self.net & mask(len) == other.net & mask(len)
    }

    fn within(self, outer: Cidr) -> bool {
        self.len >= outer.len && self.net & mask(outer.len) == outer.net
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.net), self.len)
    }
}

fn mask(len: u8) -> u32 {
    if len == 0 { 0 } else { u32::MAX << (32 - u32::from(len)) }
}

pub fn install() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("run with sudo: sudo berth helper install");
    }
    let uid: u32 = std::env::var("SUDO_UID")
        .context("SUDO_UID not set; run through sudo from your own account")?
        .parse()?;
    if uid == 0 {
        bail!("run sudo from your own account, not from a root shell");
    }

    unload();
    std::fs::create_dir_all(Path::new(BINARY).parent().unwrap())?;
    // Root runs this binary, so it must not stay writable by the user: copy it
    // to a root-owned location rather than pointing launchd at the build tree.
    let tmp = format!("{BINARY}.tmp");
    std::fs::copy(std::env::current_exe()?, &tmp)?;
    std::os::unix::fs::chown(&tmp, Some(0), Some(0))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, BINARY)?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{BINARY}</string>
        <string>helper</string>
        <string>serve</string>
        <string>--uid</string>
        <string>{uid}</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardErrorPath</key><string>/var/log/dev.berth.helper.log</string>
</dict>
</plist>
"#
    );
    std::fs::write(PLIST, plist)?;
    std::fs::set_permissions(PLIST, std::fs::Permissions::from_mode(0o644))?;
    // launchd can still be tearing down the old job; retry briefly on EIO.
    let mut last = String::new();
    let loaded = (0..10).any(|_| {
        match Command::new("launchctl").args(["bootstrap", "system", PLIST]).output() {
            Ok(out) if out.status.success() => return true,
            Ok(out) => last = String::from_utf8_lossy(&out.stderr).trim().to_string(),
            Err(err) => last = err.to_string(),
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        false
    });
    if !loaded {
        bail!("launchctl bootstrap failed: {last}");
    }
    for _ in 0..20 {
        if Path::new(SOCKET).exists() {
            println!("Installed the berth route helper for uid {uid}.");
            println!("Container IPs are reachable from the Mac while berth is running.");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    bail!("helper did not start; see /var/log/dev.berth.helper.log")
}

/// Stop and unload the helper if loaded, waiting until launchd has let go of
/// it: `bootout` returns before teardown finishes, and bootstrapping again too
/// early fails with "Input/output error".
fn unload() {
    let target = format!("system/{LABEL}");
    // "No such process" on a first install is fine.
    let _ = Command::new("launchctl").args(["bootout", &target]).output();
    for _ in 0..20 {
        let loaded = Command::new("launchctl").args(["print", &target]).output().is_ok_and(|o| o.status.success());
        if !loaded {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

pub fn uninstall() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("run with sudo: sudo berth helper uninstall");
    }
    // Stopping the helper removes the routes it installed.
    unload();
    for path in [PLIST, BINARY, SOCKET] {
        let _ = std::fs::remove_file(path);
    }
    println!("Removed the berth route helper.");
    Ok(())
}

pub fn serve(uid: u32) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("the helper must run as root");
    }
    let _ = std::fs::remove_file(SOCKET);
    let listener = UnixListener::bind(SOCKET)?;
    std::os::unix::fs::chown(SOCKET, Some(uid), None)?;
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o600))?;

    // Leave no routes behind when launchd stops us. Block the signals before
    // spawning so every thread inherits the mask and only the waiter sees them.
    let signals = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    let installed = std::sync::Arc::new(std::sync::Mutex::new(BTreeSet::<Route>::new()));
    {
        let installed = installed.clone();
        std::thread::spawn(move || {
            let mut sig = 0;
            unsafe { libc::sigwait(&signals, &mut sig) };
            for route in installed.lock().unwrap().iter() {
                delete_route(route);
            }
            let _ = std::fs::remove_file(SOCKET);
            std::process::exit(0);
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Err(err) = handle(stream, uid, &mut installed.lock().unwrap()) {
            eprintln!("berth-helper: {err:#}");
        }
    }
    Ok(())
}

fn handle(stream: UnixStream, uid: u32, installed: &mut BTreeSet<Route>) -> Result<()> {
    let (mut peer_uid, mut peer_gid) = (0, 0);
    if unsafe { libc::getpeereid(std::os::fd::AsRawFd::as_raw_fd(&stream), &mut peer_uid, &mut peer_gid) } != 0
        || (peer_uid != uid && peer_uid != 0)
    {
        bail!("rejected request from uid {peer_uid}");
    }
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    if let Ok(bind) = serde_json::from_str::<BindRequest>(&line) {
        return send_bound(&stream, &bind);
    }
    let request: SetRoutes = serde_json::from_str(&line)?;
    let mut reply = reconcile(&request.routes, installed);
    if let Some(target) = &request.docker_socket
        && let Err(err) = link_docker_socket(target, uid)
    {
        reply.errors.push(format!("{DEFAULT_DOCKER_SOCKET}: {err:#}"));
    }
    if let Some(dir) = &request.cli_dir
        && let Err(err) = link_cli_tools(dir, uid)
    {
        reply.errors.push(format!("CLI links: {err:#}"));
    }
    reply.version = VERSION;
    let mut stream = stream;
    writeln!(stream, "{}", serde_json::to_string(&reply)?)?;
    Ok(())
}

fn reconcile(wanted: &[Route], installed: &mut BTreeSet<Route>) -> Reply {
    let mut reply = Reply::default();
    let mut valid = BTreeSet::new();
    let table = route_table();
    for route in wanted {
        match validate(route, &table, installed) {
            Ok(()) => {
                valid.insert(route.clone());
            }
            Err(err) => reply.errors.push(format!("{}: {err}", route.subnet)),
        }
    }
    for route in installed.difference(&valid).cloned().collect::<Vec<_>>() {
        delete_route(&route);
        installed.remove(&route);
    }
    for route in valid.difference(installed).cloned().collect::<Vec<_>>() {
        // Clear any stale copy (e.g. from a helper restart) before adding.
        let _ = Command::new("/sbin/route").args(["-n", "-q", "delete", "-net", &route.subnet]).output();
        let out = Command::new("/sbin/route").args(["-n", "-q", "add", "-net", &route.subnet, &route.gateway]).output();
        match out {
            Ok(o) if o.status.success() => {
                eprintln!("berth-helper: route {} via {}", route.subnet, route.gateway);
                installed.insert(route);
            }
            Ok(o) => reply.errors.push(format!("{}: {}", route.subnet, String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => reply.errors.push(format!("{}: {e}", route.subnet)),
        }
    }
    reply.active = installed.iter().cloned().collect();
    reply
}

/// Point /var/run/docker.sock at `target`, which may only be the authorized
/// user's berth socket. Only an existing symlink is replaced (that is how
/// OrbStack and Docker Desktop install theirs); a real socket is left alone.
/// Not undone when berth stops: like OrbStack, the last one started wins.
fn link_docker_socket(target: &str, uid: u32) -> Result<()> {
    let home = home_of(uid).context("no home directory for the helper's user")?;
    let expected = home.join(".berth/run/docker.sock");
    if Path::new(target) != expected {
        bail!("refusing to link to {target}");
    }
    match std::fs::symlink_metadata(DEFAULT_DOCKER_SOCKET) {
        Ok(meta) if !meta.file_type().is_symlink() => bail!("is not a symlink; leaving it alone"),
        Ok(_) if std::fs::read_link(DEFAULT_DOCKER_SOCKET)? == expected => return Ok(()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let tmp = format!("{DEFAULT_DOCKER_SOCKET}.berth-tmp");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&expected, &tmp)?;
    std::fs::rename(&tmp, DEFAULT_DOCKER_SOCKET)?;
    eprintln!("berth-helper: {DEFAULT_DOCKER_SOCKET} -> {}", expected.display());
    Ok(())
}

/// Link /usr/local/bin/{docker,docker-compose} to the user's berth copies,
/// only where missing or dangling: a working CLI from another app stays.
fn link_cli_tools(dir: &str, uid: u32) -> Result<()> {
    let home = home_of(uid).context("no home directory for the helper's user")?;
    let expected = home.join(".berth/bin");
    if Path::new(dir) != expected {
        bail!("refusing to link to {dir}");
    }
    std::fs::create_dir_all("/usr/local/bin")?;
    for name in crate::cli_tools::SYSTEM_LINKS {
        let link = Path::new("/usr/local/bin").join(name);
        let target = expected.join(name);
        if !target.exists() {
            continue;
        }
        let present = std::fs::symlink_metadata(&link).is_ok();
        if present && !crate::cli_tools::is_dangling(&link) {
            continue;
        }
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link)?;
        eprintln!("berth-helper: {} -> {}", link.display(), target.display());
    }
    Ok(())
}

fn home_of(uid: u32) -> Option<std::path::PathBuf> {
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
    Some(std::path::PathBuf::from(dir.to_string_lossy().into_owned()))
}

fn delete_route(route: &Route) {
    eprintln!("berth-helper: removing route {}", route.subnet);
    let _ = Command::new("/sbin/route").args(["-n", "-q", "delete", "-net", &route.subnet, &route.gateway]).output();
}

fn validate(route: &Route, table: &[(Cidr, String)], installed: &BTreeSet<Route>) -> Result<()> {
    let subnet = Cidr::parse(&route.subnet).context("not a network address in CIDR form")?;
    if !(8..=30).contains(&subnet.len) {
        bail!("prefix length must be 8-30");
    }
    if !ALLOWED.iter().any(|&(net, len)| subnet.within(Cidr { net: u32::from(net), len })) {
        bail!("not a private range");
    }
    let gateway: Ipv4Addr = route.gateway.parse().context("bad gateway")?;
    match interface_for(gateway) {
        Some(iface) if iface.starts_with("bridge") => {}
        Some(iface) => bail!("gateway {gateway} is on {iface}, not a VM bridge"),
        None => bail!("gateway {gateway} is not reachable"),
    }
    let ours: BTreeSet<Cidr> = installed.iter().filter_map(|r| Cidr::parse(&r.subnet)).collect();
    if let Some((existing, iface)) =
        table.iter().find(|(c, _)| !ours.contains(c) && c.overlaps(subnet))
    {
        bail!("overlaps existing route {existing} on {iface}");
    }
    Ok(())
}

fn interface_for(ip: Ipv4Addr) -> Option<String> {
    let out = Command::new("/sbin/route").args(["-n", "get", &ip.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface: ").map(str::to_string))
}

/// Network routes (not host routes, not default) from `netstat -rn`.
fn route_table() -> Vec<(Cidr, String)> {
    let Ok(out) = Command::new("/usr/sbin/netstat").args(["-rn", "-f", "inet"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (dest, flags, iface) = (fields.first()?, fields.get(2)?, fields.get(3)?);
            if *dest == "default" || flags.contains('H') {
                return None;
            }
            Some((parse_netstat_dest(dest)?, iface.to_string()))
        })
        .collect()
}

/// netstat abbreviates networks: "192.168.64" is /24, "127" is /8, and an
/// explicit "/len" may follow a shortened address ("192.168.138/23").
fn parse_netstat_dest(dest: &str) -> Option<Cidr> {
    let (addr, len) = match dest.split_once('/') {
        Some((a, l)) => (a, Some(l.parse::<u8>().ok()?)),
        None => (dest, None),
    };
    let octets: Vec<u8> = addr.split('.').map(|o| o.parse().ok()).collect::<Option<_>>()?;
    if octets.is_empty() || octets.len() > 4 {
        return None;
    }
    let mut full = [0u8; 4];
    full[..octets.len()].copy_from_slice(&octets);
    let len = len.unwrap_or(8 * octets.len() as u8);
    Some(Cidr { net: u32::from(Ipv4Addr::from(full)) & mask(len), len })
}

/// Ask the helper to make exactly `routes` active and, if given, point the
/// default Docker socket at ours. None if the helper isn't installed.
/// A request for a listening socket on a privileged loopback port.
#[derive(Serialize, Deserialize)]
struct BindRequest {
    bind: std::net::IpAddr,
    port: u16,
    udp: bool,
}

/// Bind the port and pass the socket back over `stream` (SCM_RIGHTS), or
/// reply with the error. Only loopback and only ports below 1024: anything
/// else berth can bind itself.
fn send_bound(stream: &UnixStream, req: &BindRequest) -> Result<()> {
    use std::os::fd::AsRawFd;
    let result: std::io::Result<std::os::fd::OwnedFd> = if !req.bind.is_loopback() || req.port == 0 || req.port >= 1024 {
        Err(std::io::Error::other(format!("refusing to bind {}:{}", req.bind, req.port)))
    } else if req.udp {
        std::net::UdpSocket::bind((req.bind, req.port)).map(Into::into)
    } else {
        std::net::TcpListener::bind((req.bind, req.port)).map(Into::into)
    };
    let fd = match result {
        Ok(fd) => fd,
        Err(err) => {
            let mut stream = stream;
            writeln!(stream, "error: {err}")?;
            return Ok(());
        }
    };
    let mut byte = *b"\n";
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut control = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd.as_raw_fd());
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    eprintln!("berth-helper: bound {}:{} for berth", req.bind, req.port);
    Ok(())
}

/// A socket bound by the helper on a privileged loopback port, for a port
/// forward. The TCP one is bound but not yet listening.
pub fn bind_privileged(bind: std::net::IpAddr, port: u16, udp: bool) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    if !Path::new(SOCKET).exists() {
        bail!("needs the berth helper");
    }
    let mut stream = UnixStream::connect(SOCKET)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    writeln!(stream, "{}", serde_json::to_string(&BindRequest { bind, port, udp })?)?;
    let mut buf = [0u8; 256];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut control = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if !cmsg.is_null() && (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>());
            return Ok(std::os::fd::OwnedFd::from_raw_fd(fd));
        }
    }
    let text = String::from_utf8_lossy(&buf[..n as usize]);
    bail!("{}", text.trim().strip_prefix("error: ").unwrap_or("helper did not bind (outdated helper?)"))
}

pub fn set_routes(routes: Vec<Route>, docker_socket: Option<String>, cli_dir: Option<String>) -> Option<Result<Reply>> {
    if !Path::new(SOCKET).exists() {
        return None;
    }
    Some((|| {
        let mut stream = UnixStream::connect(SOCKET)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        writeln!(stream, "{}", serde_json::to_string(&SetRoutes { routes, cli_dir, docker_socket })?)?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_rules() {
        let c = |s| Cidr::parse(s).unwrap();
        assert!(Cidr::parse("172.17.0.1/16").is_none(), "host bits set");
        assert!(c("172.17.0.0/16").within(c("172.16.0.0/12")));
        assert!(!c("172.32.0.0/16").within(c("172.16.0.0/12")));
        assert!(c("192.168.0.0/20").overlaps(c("192.168.0.0/24")));
        assert!(!c("192.168.64.0/24").overlaps(c("192.168.65.0/24")));
    }

    #[test]
    fn netstat_abbreviations() {
        let p = |s| parse_netstat_dest(s).unwrap().to_string();
        assert_eq!(p("192.168.64"), "192.168.64.0/24");
        assert_eq!(p("127"), "127.0.0.0/8");
        assert_eq!(p("169.254"), "169.254.0.0/16");
        assert_eq!(p("192.168.138/23"), "192.168.138.0/23");
        assert_eq!(p("224.0.0/4"), "224.0.0.0/4");
    }
}
