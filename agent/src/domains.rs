//! Container domains. Every running container gets a service IP on the
//! host-facing network; the guest claims that address and DNATs all traffic for
//! it to the container. The mDNS responder (mdns.rs) answers for the names.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead};
use std::net::Ipv4Addr;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::docker::{self, Container};

pub const SUFFIX: &str = "berth.local";
/// Label for extra domains: comma-separated, `*.` wildcards, `.local` only.
/// The OrbStack spelling is honored too so existing compose files work.
const DOMAIN_LABELS: [&str; 2] = ["dev.berth.domains", "dev.orbstack.domains"];
/// Marks addresses we added, so stale ones from an earlier run can be removed.
const ADDR_LABEL_SUFFIX: &str = ":berth";
const NAT_CHAIN: &str = "BERTH-SVC";

#[derive(Clone, Serialize, Debug)]
pub struct Entry {
    /// Name without trailing dot, lowercase.
    pub domain: String,
    /// Also answer for any subdomain.
    pub wildcard: bool,
    /// Only answer for subdomains (`*.foo.local` labels).
    pub subdomains_only: bool,
    pub ip: Ipv4Addr,
    pub container: String,
}

#[derive(Default)]
pub struct Table {
    pub entries: Vec<Entry>,
    /// The guest's own address, answered for the bare suffix.
    pub guest_ip: Option<Ipv4Addr>,
}

impl Table {
    /// Most specific match wins.
    pub fn lookup(&self, name: &str) -> Option<Ipv4Addr> {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        if name == SUFFIX {
            return self.guest_ip;
        }
        self.entries
            .iter()
            .filter(|e| {
                let exact = name == e.domain && !e.subdomains_only;
                let sub = (e.wildcard || e.subdomains_only)
                    && name.len() > e.domain.len()
                    && name.ends_with(&e.domain)
                    && name.as_bytes()[name.len() - e.domain.len() - 1] == b'.';
                exact || sub
            })
            .max_by_key(|e| e.domain.len())
            .map(|e| e.ip)
    }
}

pub type Shared = Arc<RwLock<Table>>;

/// Keep the table and the guest's addresses/NAT in step with Docker.
pub fn run(table: Shared) {
    let mut synced = false;
    loop {
        // Failures before the first sync are just dockerd still starting.
        if let Err(err) = watch(&table, &mut synced)
            && synced
        {
            eprintln!("berth-agent: domains: {err}");
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn watch(table: &Shared, synced: &mut bool) -> io::Result<()> {
    let filters = r#"{"type":["container","network"]}"#;
    let mut events = docker::get(&format!("/events?filters={}", percent_encode(filters)))?;
    sync(table)?;
    *synced = true;
    let mut line = String::new();
    loop {
        line.clear();
        if events.read_line(&mut line)? == 0 {
            return Err(io::Error::other("docker event stream ended"));
        }
        // Events come in bursts (create, connect, start); settle briefly.
        thread::sleep(Duration::from_millis(100));
        sync(table)?;
    }
}

#[derive(Deserialize)]
struct IpAddr {
    ifname: String,
    addr_info: Vec<AddrInfo>,
}

#[derive(Deserialize)]
struct AddrInfo {
    family: String,
    local: String,
    prefixlen: u8,
    #[serde(default)]
    label: Option<String>,
}

struct Uplink {
    ifname: String,
    ip: Ipv4Addr,
    prefix: u8,
    /// Addresses we added earlier (labelled `<if>:berth`).
    ours: BTreeSet<Ipv4Addr>,
}

/// The interface carrying the default route, i.e. the one facing the Mac.
fn uplink() -> io::Result<Uplink> {
    let routes = std::fs::read_to_string("/proc/net/route")?;
    let ifname = routes
        .lines()
        .skip(1)
        .find_map(|l| {
            let mut f = l.split_whitespace();
            let name = f.next()?;
            (f.next()? == "00000000").then(|| name.to_string())
        })
        .ok_or_else(|| io::Error::other("no default route"))?;
    let out = Command::new("ip").args(["-j", "-4", "addr", "show", "dev", &ifname]).output()?;
    let addrs: Vec<IpAddr> = serde_json::from_slice(&out.stdout).map_err(io::Error::other)?;
    let infos = addrs.into_iter().filter(|a| a.ifname == ifname).flat_map(|a| a.addr_info);
    let ours_label = format!("{ifname}{ADDR_LABEL_SUFFIX}");
    let mut primary = None;
    let mut ours = BTreeSet::new();
    for info in infos.filter(|i| i.family == "inet") {
        let Ok(ip) = info.local.parse() else { continue };
        if info.label.as_deref() == Some(ours_label.as_str()) {
            ours.insert(ip);
        } else if primary.is_none() {
            primary = Some((ip, info.prefixlen));
        }
    }
    let (ip, prefix) = primary.ok_or_else(|| io::Error::other(format!("{ifname} has no IPv4 address")))?;
    Ok(Uplink { ifname, ip, prefix, ours })
}

/// Service IPs come from the top quarter of the uplink subnet, away from the
/// DHCP server, which hands out addresses from the bottom.
fn pool(up: &Uplink) -> Vec<Ipv4Addr> {
    let host_bits = 32 - u32::from(up.prefix.min(30));
    let size = 1u32 << host_bits;
    let base = u32::from(up.ip) & !(size - 1);
    let start = base + size - size / 4;
    (start..base + size - 1)
        .map(Ipv4Addr::from)
        .filter(|&ip| ip != up.ip)
        .collect()
}

fn names_for(c: &Container) -> Vec<(String, bool, bool)> {
    let mut names = Vec::new();
    if let Some(name) = c.name() {
        names.push((format!("{}.{SUFFIX}", name.to_ascii_lowercase()), true, false));
    }
    if let (Some(service), Some(project)) =
        (c.labels.get("com.docker.compose.service"), c.labels.get("com.docker.compose.project"))
    {
        names.push((format!("{service}.{project}.{SUFFIX}").to_ascii_lowercase(), true, false));
    }
    for label in DOMAIN_LABELS {
        for domain in c.labels.get(label).into_iter().flat_map(|v| v.split(',')) {
            // `name@port[:https]` routing hints are for an HTTP proxy we don't
            // have yet; the name part still resolves.
            let domain = domain.split('@').next().unwrap_or("").trim().to_ascii_lowercase();
            let (domain, subs) = match domain.strip_prefix("*.") {
                Some(rest) => (rest.to_string(), true),
                None => (domain, false),
            };
            if domain.ends_with(".local") {
                names.push((domain, false, subs));
            }
        }
    }
    names
}

fn sync(table: &Shared) -> io::Result<()> {
    let up = uplink()?;
    let pool = pool(&up);
    let containers = docker::containers()?;

    // Stable assignment: hash the container name into the pool and probe, so a
    // container keeps its IP across restarts unless there is a collision.
    let mut taken = BTreeSet::new();
    let mut mapping = BTreeMap::new();
    let mut entries = Vec::new();
    let mut sorted: Vec<&Container> = containers.iter().filter(|c| c.ip().is_some()).collect();
    sorted.sort_by(|a, b| a.name().cmp(&b.name()));
    for c in sorted {
        let (Some(name), Some(target)) = (c.name(), c.ip()) else { continue };
        let start = fnv(name) as usize % pool.len().max(1);
        let Some(svc) = (0..pool.len()).map(|i| pool[(start + i) % pool.len()]).find(|ip| !taken.contains(ip))
        else {
            eprintln!("berth-agent: service IP pool exhausted; {name} gets no domain");
            continue;
        };
        taken.insert(svc);
        mapping.insert(svc, target);
        for (domain, wildcard, subdomains_only) in names_for(c) {
            entries.push(Entry { domain, wildcard, subdomains_only, ip: svc, container: name.to_string() });
        }
    }

    apply(&up, &mapping)?;
    let gone: Vec<(String, Ipv4Addr)> = {
        let mut t = table.write().unwrap();
        let gone = t
            .entries
            .iter()
            .filter(|old| !old.subdomains_only)
            .filter(|old| !entries.iter().any(|new| new.domain == old.domain && new.ip == old.ip))
            .map(|old| (old.domain.clone(), old.ip))
            .collect();
        t.entries = entries;
        t.guest_ip = Some(up.ip);
        gone
    };
    if let Err(err) = crate::mdns::goodbye(&gone, up.ip) {
        eprintln!("berth-agent: mdns goodbye: {err}");
    }
    Ok(())
}

/// Claim service IPs on the uplink and DNAT them to their containers.
fn apply(up: &Uplink, mapping: &BTreeMap<Ipv4Addr, Ipv4Addr>) -> io::Result<()> {
    let label = format!("{}{ADDR_LABEL_SUFFIX}", up.ifname);
    for ip in up.ours.iter().filter(|ip| !mapping.contains_key(ip)) {
        exec("ip", &["addr", "del", &format!("{ip}/32"), "dev", &up.ifname]);
    }
    for ip in mapping.keys().filter(|ip| !up.ours.contains(ip)) {
        exec("ip", &["addr", "add", &format!("{ip}/32"), "dev", &up.ifname, "label", &label]);
    }

    // Our NAT chain runs first in PREROUTING; rebuilt on every sync.
    run_quiet("iptables", &["-t", "nat", "-N", NAT_CHAIN]);
    if !run_quiet("iptables", &["-t", "nat", "-C", "PREROUTING", "-j", NAT_CHAIN]) {
        exec("iptables", &["-t", "nat", "-I", "PREROUTING", "1", "-j", NAT_CHAIN]);
    }
    exec("iptables", &["-t", "nat", "-F", NAT_CHAIN]);
    for (svc, target) in mapping {
        exec(
            "iptables",
            &["-t", "nat", "-A", NAT_CHAIN, "-d", &format!("{svc}/32"), "-j", "DNAT", "--to-destination", &target.to_string()],
        );
    }
    // Docker's FORWARD policy drops new connections to unpublished container
    // ports; let through anything we (or Docker) DNATed.
    let accept = ["DOCKER-USER", "-m", "conntrack", "--ctstate", "DNAT", "-j", "ACCEPT"];
    if !run_quiet("iptables", &[&["-C"], &accept[..]].concat()) {
        exec("iptables", &[&["-I"], &accept[..]].concat());
    }
    Ok(())
}

fn exec(cmd: &str, args: &[&str]) -> bool {
    match Command::new(cmd).args(args).output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            eprintln!("berth-agent: {cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
            false
        }
        Err(err) => {
            eprintln!("berth-agent: {cmd}: {err}");
            false
        }
    }
}

fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success())
}

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x01000193))
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(domain: &str, wildcard: bool, subdomains_only: bool, last: u8) -> Entry {
        Entry {
            domain: domain.into(),
            wildcard,
            subdomains_only,
            ip: Ipv4Addr::new(10, 0, 0, last),
            container: "c".into(),
        }
    }

    #[test]
    fn lookup_rules() {
        let t = Table {
            entries: vec![
                entry("web.berth.local", true, false, 1),
                entry("api.web.berth.local", true, false, 2),
                entry("foo.local", false, true, 3),
            ],
            guest_ip: Some(Ipv4Addr::new(10, 0, 0, 9)),
        };
        let ip = |n| t.lookup(n).map(|ip| ip.octets()[3]);
        assert_eq!(ip("web.berth.local"), Some(1));
        assert_eq!(ip("WEB.berth.local."), Some(1));
        assert_eq!(ip("x.web.berth.local"), Some(1));
        assert_eq!(ip("api.web.berth.local"), Some(2));
        assert_eq!(ip("v1.api.web.berth.local"), Some(2));
        assert_eq!(ip("xweb.berth.local"), None);
        assert_eq!(ip("foo.local"), None);
        assert_eq!(ip("a.foo.local"), Some(3));
        assert_eq!(ip("berth.local"), Some(9));
    }

    #[test]
    fn pool_is_top_quarter() {
        let up = Uplink { ifname: "e".into(), ip: Ipv4Addr::new(192, 168, 64, 4), prefix: 24, ours: BTreeSet::new() };
        let p = pool(&up);
        assert_eq!(p.first(), Some(&Ipv4Addr::new(192, 168, 64, 192)));
        assert_eq!(p.last(), Some(&Ipv4Addr::new(192, 168, 64, 254)));
    }
}
