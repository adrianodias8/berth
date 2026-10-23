//! Linux machines: full distros (systemd, your user, your Mac home folder)
//! running as systemd-nspawn containers on the guest's kernel. Root
//! filesystems come from images.linuxcontainers.org, checksum-verified.
//!
//! Each machine gets a veth NIC named eth0 inside (so the distro's own network
//! config applies) and ve-<name> outside, which systemd-networkd's stock
//! 80-container-ve.network serves with DHCP and NAT.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;

use serde::Serialize;

const IMAGES: &str = "https://images.linuxcontainers.org";
const MACHINES: &str = "/var/lib/machines";
const DEFAULT_DISTRO: &str = "ubuntu:noble";

fn sh(script: &str) -> io::Result<String> {
    let out = Command::new("sh").args(["-c", script]).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(err.lines().last().unwrap_or("command failed").to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn valid_name(name: &str) -> bool {
    // ve-<name> must fit in a 15-character interface name.
    (1..=12).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
}

fn ensure_tools() -> io::Result<()> {
    if Path::new("/usr/bin/machinectl").exists() {
        return Ok(());
    }
    sh("DEBIAN_FRONTEND=noninteractive apt-get install -y -q systemd-container xz-utils >/dev/null").map(|_| ())
}

/// Docker's FORWARD policy is DROP; let machine traffic through.
fn allow_forwarding() {
    for rule in [["-i", "ve-+"], ["-o", "ve-+"]] {
        let check = Command::new("iptables").args(["-w", "-C", "DOCKER-USER", rule[0], rule[1], "-j", "ACCEPT"]).output();
        if !check.is_ok_and(|o| o.status.success()) {
            let _ = Command::new("iptables").args(["-w", "-I", "DOCKER-USER", rule[0], rule[1], "-j", "ACCEPT"]).output();
        }
    }
}

/// `distro[:release]` → the image directory path from the index.
fn image_path(spec: &str) -> io::Result<String> {
    let (distro, release) = spec.split_once(':').unwrap_or((spec, ""));
    let index = sh(&format!("curl -fsSL {IMAGES}/meta/1.0/index-system"))?;
    let candidates: Vec<(&str, &str)> = index
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(';').collect();
            (f.len() >= 6 && f[0] == distro && f[2] == "arm64" && f[3] == "default").then(|| (f[1], f[5]))
        })
        .collect();
    if candidates.is_empty() {
        return Err(io::Error::other(format!("no arm64 image for {distro}")));
    }
    // Without a release, the newest stable one: not a development branch
    // (alpine edge, debian forky/sid) unless that's all there is.
    let unstable = ["edge", "sid", "forky", "rawhide"];
    let stable: Vec<&(&str, &str)> = candidates.iter().filter(|(r, _)| !unstable.contains(r)).collect();
    let chosen = if release.is_empty() {
        stable.last().copied().or(candidates.last())
    } else {
        candidates.iter().find(|(r, _)| *r == release)
    };
    chosen
        .map(|(_, path)| path.to_string())
        .ok_or_else(|| io::Error::other(format!("{distro} has no release {release}; try one of: {}", candidates.iter().map(|c| c.0).collect::<Vec<_>>().join(", "))))
}

fn create(name: &str, spec: &str, user: &str, uid: &str, home: &str) -> io::Result<()> {
    if !valid_name(name) {
        return Err(io::Error::other("name: 1-12 characters, a-z 0-9 and -"));
    }
    let root = format!("{MACHINES}/{name}");
    if Path::new(&root).exists() {
        return Err(io::Error::other(format!("machine {name} already exists")));
    }
    ensure_tools()?;
    let path = image_path(if spec.is_empty() { DEFAULT_DISTRO } else { spec })?;
    let tarball = format!("{MACHINES}/.{name}.tar.xz");
    sh(&format!(
        "set -e; mkdir -p {MACHINES}; curl -fsSL -o {tarball} '{IMAGES}{path}rootfs.tar.xz'; \
         want=$(curl -fsSL '{IMAGES}{path}SHA256SUMS' | awk '$2==\"rootfs.tar.xz\"{{print $1}}'); \
         echo \"$want  {tarball}\" | sha256sum -c - >/dev/null; \
         mkdir {root}; tar -xJf {tarball} -C {root}; rm -f {tarball}"
    ))
    .inspect_err(|_| {
        let _ = std::fs::remove_file(&tarball);
        let _ = std::fs::remove_dir_all(&root);
    })?;

    // The user, with the same UID as on the Mac so files in the shared home
    // keep their ownership; passwordless sudo where sudo exists.
    let setup = format!(
        "set -e; \
         if command -v useradd >/dev/null; then useradd -m -u {uid} -s /bin/sh {user} 2>/dev/null || true; \
         else adduser -D -u {uid} {user} 2>/dev/null || true; fi; \
         for g in sudo wheel; do getent group $g >/dev/null && (usermod -aG $g {user} 2>/dev/null || addgroup {user} $g 2>/dev/null) || true; done; \
         command -v sudo >/dev/null || apk add -q sudo 2>/dev/null || (apt-get update -qq && apt-get install -y -qq sudo) 2>/dev/null \
             || dnf install -y -q sudo 2>/dev/null || pacman -Sy --noconfirm sudo 2>/dev/null || zypper -q in -y sudo 2>/dev/null || true; \
         mkdir -p /etc/sudoers.d; echo '{user} ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/berth; chmod 440 /etc/sudoers.d/berth; \
         command -v bash >/dev/null && (chsh -s \"$(command -v bash)\" {user} 2>/dev/null || true); \
         echo {name} > /etc/hostname"
    );
    sh(&format!("systemd-nspawn -q -D {root} --pipe /bin/sh -c '{}'", setup.replace('\'', "'\\''")))?;

    let ve = format!("ve-{name}");
    std::fs::create_dir_all("/etc/systemd/nspawn")?;
    std::fs::write(
        format!("/etc/systemd/nspawn/{name}.nspawn"),
        format!("[Exec]\nBoot=yes\nPrivateUsers=no\n\n[Files]\nBind={home}\n\n[Network]\nVirtualEthernetExtra={ve}:eth0\n"),
    )?;
    allow_forwarding();
    sh(&format!("machinectl enable {name} >/dev/null 2>&1; machinectl start {name}"))?;
    Ok(())
}

#[derive(Serialize)]
struct Machine {
    name: String,
    state: String,
    os: String,
    addresses: Vec<String>,
}

fn list() -> io::Result<Vec<Machine>> {
    if !Path::new("/usr/bin/machinectl").exists() {
        return Ok(Vec::new());
    }
    // Running machines and their addresses ("a\nb" per machine).
    let running: Vec<(String, Vec<String>)> = sh("machinectl list --output=json 2>/dev/null")
        .ok()
        .and_then(|j| serde_json::from_str::<Vec<serde_json::Value>>(&j).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            let name = m["machine"].as_str()?.to_string();
            let addrs = m["addresses"].as_str().unwrap_or("").lines().filter(|a| !a.contains(':')).map(str::to_string).collect();
            Some((name, addrs))
        })
        .collect();
    let mut machines = Vec::new();
    for entry in std::fs::read_dir(MACHINES).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        let os = std::fs::read_to_string(entry.path().join("etc/os-release"))
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
            .unwrap_or_default();
        let entry_running = running.iter().find(|(n, _)| *n == name);
        let up = entry_running.is_some();
        let addresses = entry_running.map(|(_, a)| a.clone()).unwrap_or_default();
        machines.push(Machine { name, state: if up { "running" } else { "stopped" }.into(), os, addresses });
    }
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(machines)
}

/// `machine <action>` followed by a JSON array of arguments until EOF.
/// Replies `ok`, `error: ...`, or (for list) JSON.
pub fn handle(action: &str, mut host: UnixStream) -> io::Result<()> {
    let mut body = String::new();
    host.read_to_string(&mut body)?;
    let args: Vec<String> = serde_json::from_str(&body).unwrap_or_default();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    let result: io::Result<String> = match action {
        "create" => create(arg(0), arg(1), arg(2), arg(3), arg(4)).map(|_| "ok".into()),
        "start" => sh(&format!("machinectl start {}", arg(0))).map(|_| "ok".into()),
        "stop" => sh(&format!("machinectl poweroff {}", arg(0))).map(|_| "ok".into()),
        "delete" => {
            let name = arg(0);
            if !valid_name(name) {
                Err(io::Error::other("bad name"))
            } else {
                let _ = sh(&format!("machinectl disable {name} 2>/dev/null; machinectl terminate {name} 2>/dev/null; true"));
                // Wait until it's really gone; its files stay busy until then.
                for _ in 0..50 {
                    if sh(&format!("machinectl show {name} >/dev/null 2>&1")).is_err() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                let _ = std::fs::remove_file(format!("/etc/systemd/nspawn/{name}.nspawn"));
                std::fs::remove_dir_all(format!("{MACHINES}/{name}")).map(|_| "ok".into())
            }
        }
        "list" => list().and_then(|m| serde_json::to_string(&m).map_err(io::Error::other)),
        _ => Err(io::Error::other(format!("unknown action {action}"))),
    };
    if matches!(action, "create" | "start") && result.is_ok() {
        // Domains need the machine's address, which DHCP hands out a moment
        // after boot.
        let name = arg(0).to_string();
        std::thread::spawn(move || {
            for _ in 0..30 {
                if running_addresses().iter().any(|(n, _)| *n == name) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            crate::domains::request_sync();
        });
    } else if matches!(action, "stop" | "delete") {
        crate::domains::request_sync();
    }
    let reply = result.unwrap_or_else(|e| format!("error: {e}"));
    host.write_all(reply.as_bytes())
}

/// Running machines and their addresses, for container-style domains.
pub fn running_addresses() -> Vec<(String, std::net::Ipv4Addr)> {
    list()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| Some((m.name, m.addresses.first()?.parse().ok()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("ubuntu"));
        assert!(valid_name("dev-box-2"));
        assert!(!valid_name("Ubuntu"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("thirteen-char"));
        assert!(!valid_name("a;rm -rf"));
    }
}
