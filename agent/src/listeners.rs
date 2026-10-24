//! Host networking: servers in `--net host` containers listen in the guest's
//! own network namespace, where no published port describes them. We poll the
//! root namespace's socket tables and keep the listeners owned by a process in
//! a container's cgroup. That leaves out everything berth and Debian run
//! themselves (sshd, the agent, dockerd and its port proxies, k3s, DNS) without
//! a list of ports to exclude. When the set changes we ask the host to re-sync;
//! it reads the set with `info listeners` and forwards each port from the Mac's
//! localhost like a published one (src/ports.rs).
//!
//! Containers with their own network namespace never show up here, so any
//! container socket in these tables belongs to a host-network container (or a
//! Kubernetes pod with hostNetwork).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

/// Socket tables are a few KB; reading them every two seconds costs nothing
/// measurable, and a new server is reachable from the Mac within that time.
const POLL: Duration = Duration::from_secs(2);

#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
}

/// A listening socket: protocol, port, and the address it's bound to
/// ("0.0.0.0", "::", "127.0.0.1" or "::1"; others aren't forwarded).
#[derive(Serialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Listener {
    pub proto: Proto,
    pub port: u16,
    pub bind: String,
}

static CURRENT: Mutex<Vec<Listener>> = Mutex::new(Vec::new());

/// The host-network listeners as of the last poll (`info listeners`).
pub fn current() -> Vec<Listener> {
    CURRENT.lock().unwrap().clone()
}

pub fn run() {
    // Socket inode -> owned by a container process. Inodes aren't reused
    // while the socket exists, so only new sockets need the /proc walk.
    let mut owners: HashMap<u64, bool> = HashMap::new();
    loop {
        // Before the owner lookup: short-lived client sockets would trigger
        // a /proc walk on almost every poll.
        let ephemeral = ephemeral_ports();
        let mut sockets = read_tables();
        sockets.retain(|s| !(s.listener.proto == Proto::Udp && ephemeral.contains(&s.listener.port)));
        let unknown: HashSet<u64> = sockets.iter().map(|s| s.inode).filter(|i| !owners.contains_key(i)).collect();
        if !unknown.is_empty() {
            let containers = container_sockets();
            for inode in unknown {
                owners.insert(inode, containers.contains(&inode));
            }
        }
        let live: HashSet<u64> = sockets.iter().map(|s| s.inode).collect();
        owners.retain(|inode, _| live.contains(inode));

        let found: BTreeSet<Listener> =
            sockets.into_iter().filter(|s| owners.get(&s.inode) == Some(&true)).map(|s| s.listener).collect();
        let found: Vec<Listener> = found.into_iter().collect();
        let changed = {
            let mut current = CURRENT.lock().unwrap();
            let changed = *current != found;
            *current = found;
            changed
        };
        if changed {
            crate::engines::notify_host_sync();
        }
        std::thread::sleep(POLL);
    }
}

struct Socket {
    listener: Listener,
    inode: u64,
}

fn read_tables() -> Vec<Socket> {
    let mut sockets = Vec::new();
    for (table, proto) in [("tcp", Proto::Tcp), ("tcp6", Proto::Tcp), ("udp", Proto::Udp), ("udp6", Proto::Udp)] {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/net/{table}")) {
            sockets.extend(parse_table(&text, proto));
        }
    }
    sockets
}

/// Listening sockets in a /proc/net/{tcp,tcp6,udp,udp6} table: TCP in LISTEN,
/// UDP unconnected (state 07, no remote port), on a wildcard or loopback
/// address.
fn parse_table(text: &str, proto: Proto) -> Vec<Socket> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (local, remote, state, inode) = (fields.get(1)?, fields.get(2)?, fields.get(3)?, fields.get(9)?);
            let listening = match proto {
                Proto::Tcp => *state == "0A",
                Proto::Udp => *state == "07" && remote.ends_with(":0000"),
            };
            if !listening {
                return None;
            }
            let (addr, port) = local.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok().filter(|&p| p != 0)?;
            let bind = bind_name(parse_addr(addr)?)?;
            let inode = inode.parse().ok().filter(|&i| i != 0)?;
            Some(Socket { listener: Listener { proto, port, bind: bind.to_string() }, inode })
        })
        .collect()
}

/// The kernel prints each 32-bit word of the address as it sits in memory,
/// read as a native (little-endian) integer: 127.0.0.1 is "0100007F".
fn parse_addr(hex: &str) -> Option<IpAddr> {
    let word = |s: &str| u32::from_str_radix(s, 16).ok().map(u32::to_le_bytes);
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(word(hex)?))),
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word(&hex[i * 8..i * 8 + 8])?);
            }
            Some(IpAddr::V6(Ipv6Addr::from(bytes)))
        }
        _ => None,
    }
}

/// Wildcard and loopback binds; a server bound to one specific guest
/// address (including other 127/8 ones, like systemd-resolved's
/// 127.0.0.53) has nothing sensible to map to on the Mac.
fn bind_name(ip: IpAddr) -> Option<&'static str> {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => Some("0.0.0.0"),
        IpAddr::V4(v4) if v4 == Ipv4Addr::LOCALHOST => Some("127.0.0.1"),
        IpAddr::V6(v6) if v6.is_unspecified() => Some("::"),
        IpAddr::V6(v6) if v6.is_loopback() => Some("::1"),
        _ => None,
    }
}

/// Socket inodes held by processes in a container cgroup.
fn container_sockets() -> HashSet<u64> {
    let mut inodes = HashSet::new();
    // Processes in their own network namespace can't own a socket in our
    // tables: skip reading their descriptors.
    let ours = std::fs::read_link("/proc/self/ns/net").ok();
    let Ok(procs) = std::fs::read_dir("/proc") else { return inodes };
    for entry in procs.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().filter(|n| n.bytes().all(|b| b.is_ascii_digit())) else { continue };
        let Ok(cgroup) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) else { continue };
        if !cgroup.lines().any(|line| line.rsplit(':').next().is_some_and(is_container_cgroup)) {
            continue;
        }
        if ours.is_some() && std::fs::read_link(format!("/proc/{pid}/ns/net")).ok() != ours {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path())
                && let Some(inode) = link.to_str().and_then(|l| l.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok())
            {
                inodes.insert(inode);
            }
        }
    }
    inodes
}

/// A cgroup path that belongs to a container: Docker's
/// `docker-<id>.scope` (systemd driver, also under Kubernetes' kubepods
/// slices) or `/docker/<id>` (cgroupfs driver), Podman's `libpod-<id>.scope`
/// (but not its `libpod-conmon-` monitor), nerdctl's `nerdctl-<id>.scope`.
fn is_container_cgroup(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments.iter().enumerate().any(|(i, seg)| {
        let scope = |prefix: &str| seg.strip_prefix(prefix).and_then(|s| s.strip_suffix(".scope")).is_some_and(is_id);
        scope("docker-")
            || scope("libpod-")
            || scope("nerdctl-")
            || scope("cri-containerd-")
            || (i > 0 && segments[i - 1] == "docker" && is_id(seg))
    })
}

fn is_id(s: &str) -> bool {
    s.len() >= 12 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The range the kernel picks unbound UDP sockets' ports from. A client
/// socket that sends without binding shows up unconnected on such a port, so
/// UDP listeners there are left out rather than forwarded for a moment.
fn ephemeral_ports() -> std::ops::RangeInclusive<u16> {
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap_or_default();
    let mut parts = range.split_whitespace().filter_map(|p| p.parse().ok());
    match (parts.next(), parts.next()) {
        (Some(low), Some(high)) => low..=high,
        _ => 32768..=60999,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listeners(text: &str, proto: Proto) -> Vec<(u16, String, u64)> {
        parse_table(text, proto).into_iter().map(|s| (s.listener.port, s.listener.bind, s.inode)).collect()
    }

    #[test]
    fn tcp_listeners_by_bind_address() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
           0: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 185657 1 0 100 0 0 10 0\n\
           1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1001 1 0 100 0 0 10 0\n\
           2: 0640A8C0:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1002 1 0 100 0 0 10 0\n\
           4: 3500007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1004 1 0 100 0 0 10 0\n\
           3: 0100007F:1F90 0100007F:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 1003 1 0 100 0 0 10 0\n";
        assert_eq!(listeners(tcp, Proto::Tcp), vec![(80, "0.0.0.0".into(), 185657), (8080, "127.0.0.1".into(), 1001)]);
        let tcp6 = "  sl  local_address remote_address st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
           0: 00000000000000000000000000000000:0050 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2001 1 0\n\
           1: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2002 1 0\n\
           2: 0000000000000000FFFF00000100007F:1389 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2003 1 0\n";
        assert_eq!(
            listeners(tcp6, Proto::Tcp),
            vec![(80, "::".into(), 2001), (3000, "::1".into(), 2002), (5001, "127.0.0.1".into(), 2003)]
        );
    }

    #[test]
    fn udp_only_unconnected() {
        let udp = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n\
           0: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 3001 2 0 0\n\
           1: 0640A8C0:D431 0140A8C0:0035 01 00000000:00000000 00:00000000 00000000     0        0 3002 2 0 0\n";
        assert_eq!(listeners(udp, Proto::Udp), vec![(5353, "0.0.0.0".into(), 3001)]);
    }

    /// Against the running guest, as root: `sudo <test binary> --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_guest() {
        let containers = container_sockets();
        for s in read_tables() {
            let mark = if containers.contains(&s.inode) { "container" } else { "-" };
            println!("{:?} {} {} {mark}", s.listener.proto, s.listener.bind, s.listener.port);
        }
    }

    #[test]
    fn container_cgroups() {
        let id = "56aef95f1e02071ef54d9c88170fad584769b72b388dbaa8908dfcd6098f3818";
        assert!(is_container_cgroup(&format!("/system.slice/docker-{id}.scope")));
        assert!(is_container_cgroup(&format!("/docker/{id}")));
        assert!(is_container_cgroup(&format!("/machine.slice/libpod-{id}.scope/container")));
        assert!(is_container_cgroup(&format!("/system.slice/nerdctl-{id}.scope")));
        assert!(is_container_cgroup(&format!("/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod1.slice/docker-{id}.scope")));
        assert!(!is_container_cgroup(&format!("/machine.slice/libpod-conmon-{id}.scope")));
        assert!(!is_container_cgroup("/system.slice/docker.service"));
        assert!(!is_container_cgroup("/system.slice/ssh.service"));
        assert!(!is_container_cgroup("/system.slice/berth-agent.service"));
        assert!(!is_container_cgroup("/user.slice/user-501.slice/session-17.scope"));
        assert!(!is_container_cgroup("/"));
    }
}
