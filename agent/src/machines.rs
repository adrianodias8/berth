//! Linux machines: full distros (systemd, your user, your Mac home folder)
//! running as systemd-nspawn containers on the guest's kernel. Root
//! filesystems come from images.linuxcontainers.org, checksum-verified.
//!
//! Each machine gets a veth NIC named eth0 inside (so the distro's own network
//! config applies) and ve-<name> outside, which systemd-networkd's stock
//! 80-container-ve.network serves with DHCP and NAT.
//!
//! Integration with the Mac (mac.rs): /run/berth is bind-mounted in, with the
//! `mac` command's and the SSH agent's sockets; `/usr/local/bin/mac` links to
//! the agent; and sshd is installed on first `ssh <name>.berth`.

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
        format!(
            "[Exec]\nBoot=yes\nPrivateUsers=no\n{}\n[Files]\nBind={home}\nBindReadOnly={}\n\n[Network]\nVirtualEthernetExtra={ve}:eth0\n",
            kill_signal(name),
            crate::mac::DIR
        ),
    )?;
    integrate_files(name);
    allow_forwarding();
    sh(&format!("machinectl enable {name} >/dev/null 2>&1; machinectl start {name}"))?;
    Ok(())
}

/// How nspawn asks the machine's init to shut down, as a .nspawn line. Its
/// default is systemd's SIGRTMIN+3, which busybox init (Alpine) ignores:
/// that machine would keep running until killed. Busybox powers off on
/// SIGUSR2.
fn kill_signal(name: &str) -> &'static str {
    let init = std::fs::read_link(format!("{MACHINES}/{name}/sbin/init")).unwrap_or_default();
    if init.file_name().is_some_and(|f| f == "busybox") { "KillSignal=SIGUSR2\n" } else { "" }
}

/// Stop a machine through its unit: nspawn sends the init its KillSignal
/// and waits (`machinectl poweroff` sends systemd's own signal instead).
pub fn stop(name: &str) -> io::Result<()> {
    if !valid_name(name) {
        return Err(io::Error::other("bad name"));
    }
    sh(&format!("systemctl stop systemd-nspawn@{name}.service")).map(|_| ())
}

pub fn start(name: &str) -> io::Result<()> {
    if !valid_name(name) {
        return Err(io::Error::other("bad name"));
    }
    allow_forwarding();
    sh(&format!("machinectl start {name}")).map(|_| ())
}

/// The `mac` link and SSH_AUTH_SOCK, in the machine's files (it needn't run).
fn integrate_files(name: &str) {
    let root = format!("{MACHINES}/{name}");
    if let Ok(agent) = std::env::current_exe() {
        // The agent runs from the shared home, which machines see at the same path.
        let link = format!("{root}/usr/local/bin/mac");
        if std::fs::read_link(&link).ok().as_deref() != Some(agent.as_path()) {
            let _ = std::fs::create_dir_all(format!("{root}/usr/local/bin"));
            let _ = std::fs::remove_file(&link);
            let _ = std::os::unix::fs::symlink(&agent, &link);
        }
    }
    let profile = format!(
        "# berth: the Mac's SSH agent, unless the session brought its own.\n\
         [ -n \"$SSH_AUTH_SOCK\" ] || [ ! -S {0}/ssh-agent.sock ] || export SSH_AUTH_SOCK={0}/ssh-agent.sock\n",
        crate::mac::DIR
    );
    let _ = std::fs::create_dir_all(format!("{root}/etc/profile.d"));
    let _ = std::fs::write(format!("{root}/etc/profile.d/berth.sh"), profile);
}

/// Bring machines made before an integration feature up to date: files,
/// the /run/berth bind in their .nspawn file and, if running, live.
pub fn integrate_all() {
    let Ok(entries) = std::fs::read_dir(MACHINES) else { return };
    // iptables rules don't survive a reboot; Docker makes DOCKER-USER once
    // it's up, which may be a while on boot.
    std::thread::spawn(|| {
        for _ in 0..120 {
            if Command::new("iptables").args(["-w", "-L", "DOCKER-USER", "-n"]).output().is_ok_and(|o| o.status.success()) {
                allow_forwarding();
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !valid_name(&name) || !entry.path().is_dir() {
            continue;
        }
        integrate_files(&name);
        let nspawn = format!("/etc/systemd/nspawn/{name}.nspawn");
        let bind = format!("BindReadOnly={}", crate::mac::DIR);
        if let Ok(config) = std::fs::read_to_string(&nspawn) {
            let mut updated = config.clone();
            if !updated.contains(&bind) {
                updated = updated.replacen("[Files]\n", &format!("[Files]\n{bind}\n"), 1);
            }
            let signal = kill_signal(&name);
            if !signal.is_empty() && !updated.contains("KillSignal=") {
                updated = updated.replacen("[Exec]\n", &format!("[Exec]\n{signal}"), 1);
            }
            if updated != config {
                let _ = std::fs::write(&nspawn, updated);
            }
        }
        let dir = crate::mac::DIR;
        let _ = sh(&format!(
            "leader=$(machinectl show {name} -p Leader --value 2>/dev/null) && [ -n \"$leader\" ] && \
             ! nsenter -t \"$leader\" -m -- test -S {dir}/mac.sock && machinectl bind --read-only --mkdir {name} {dir}; true"
        ));
    }
}

/// Install and start sshd in a running machine, with the berth key for the
/// user, for `ssh <name>.berth` (VS Code's Remote-SSH included).
pub fn ensure_sshd(name: &str) -> io::Result<()> {
    if !valid_name(name) {
        return Err(io::Error::other("bad name"));
    }
    let root = format!("{MACHINES}/{name}");
    // The machine's user (create() wrote it there) and the key the Mac uses
    // for the VM, from that user's VM account.
    let user = std::fs::read_to_string(format!("{root}/etc/sudoers.d/berth"))
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .filter(|u| u.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')))
        .ok_or_else(|| io::Error::other("no berth user in the machine"))?;
    let key = std::fs::read_to_string(format!("/home/{user}/.ssh/authorized_keys"))
        .ok()
        .and_then(|k| k.lines().find(|l| l.starts_with("ssh-")).map(str::to_string))
        .filter(|k| k.bytes().all(|b| b.is_ascii_alphanumeric() || b" +/=@._-:".contains(&b)))
        .ok_or_else(|| io::Error::other("no SSH key for the VM user"))?;
    eprintln!("berth-agent: machines: setting up sshd in {name}");
    // The agent's PATH (Debian's, merged /usr) lacks /bin, where Alpine's
    // tools are. Accounts made without a password are locked ("!"), which
    // sshd refuses even for keys: "*" is no password, not locked. sshd's
    // start goes to /dev/null: a daemon holding our pipe would keep us
    // waiting for its end.
    let script = format!(
        "set -e; export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; \
         if [ ! -x /usr/sbin/sshd ]; then \
           if command -v apt-get >/dev/null; then export DEBIAN_FRONTEND=noninteractive; apt-get update -qq; apt-get install -y -qq openssh-server >/dev/null; \
           elif command -v dnf >/dev/null; then dnf install -y -q openssh-server; \
           elif command -v apk >/dev/null; then apk add -q openssh-server; \
           elif command -v pacman >/dev/null; then pacman -Sy --noconfirm --needed openssh >/dev/null; \
           elif command -v zypper >/dev/null; then zypper -q in -y openssh-server; fi; \
         fi; \
         h=$(getent passwd {user} | cut -d: -f6); mkdir -p \"$h/.ssh\"; \
         grep -qxF '{key}' \"$h/.ssh/authorized_keys\" 2>/dev/null || echo '{key}' >> \"$h/.ssh/authorized_keys\"; \
         chown -R \"$(id -u {user}):$(id -g {user})\" \"$h/.ssh\"; chmod 700 \"$h/.ssh\"; chmod 600 \"$h/.ssh/authorized_keys\"; \
         sed -i 's/^{user}:!\\{{1,2\\}}:/{user}:*:/' /etc/shadow; \
         {{ if [ -d /run/systemd/system ]; then systemctl enable --now ssh 2>/dev/null || systemctl enable --now sshd; \
         elif command -v rc-update >/dev/null; then rc-update add sshd default; rc-service sshd start || true; fi; }} </dev/null >/dev/null 2>&1"
    );
    let run = format!("nsenter -t \"$(machinectl show {name} -p Leader --value)\" -a -- /bin/sh -c '{}'", script.replace('\'', "'\\''"));
    // A machine that just booted may not have its network yet.
    let mut result = sh(&run);
    for _ in 0..3 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
        result = sh(&run);
    }
    result.map(|_| ())
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
        "start" => start(arg(0)).map(|_| "ok".into()),
        "stop" => stop(arg(0)).map(|_| "ok".into()),
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
