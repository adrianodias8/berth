//! Linux machines: full distros (systemd, your user, your Mac home folder)
//! running as systemd-nspawn containers on the guest's kernel. Root
//! filesystems come from images.linuxcontainers.org, checksum-verified.
//!
//! Each machine gets a veth NIC named eth0 inside (so the distro's own network
//! config applies) and ve-<name> outside, which systemd-networkd's stock
//! 80-container-ve.network serves with DHCP and NAT.
//!
//! `--arch amd64` machines run on Rosetta: the VM registers it for x86-64
//! with binfmt's F flag, so it works inside machines too. `--cloud-init`
//! uses the image's "cloud" variant and seeds the user data for its
//! NoCloud datasource.
//!
//! Isolated machines (`--isolated`) get none of the Mac: no home folder, no
//! `mac` command or SSH agent, only the folders given with `--mount`.
//! `--isolate-network` also keeps them off the Mac, the LAN and other
//! private networks (DNS and the internet only). Both are recorded as
//! comments in the machine's .nspawn file; the network rules are applied
//! again at each boot.
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
fn image_path(spec: &str, arch: &str, variant: &str) -> io::Result<String> {
    let (distro, release) = spec.split_once(':').unwrap_or((spec, ""));
    let index = sh(&format!("curl -fsSL {IMAGES}/meta/1.0/index-system"))?;
    let candidates: Vec<(&str, &str)> = index
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(';').collect();
            (f.len() >= 6 && f[0] == distro && f[2] == arch && f[3] == variant).then(|| (f[1], f[5]))
        })
        .collect();
    if candidates.is_empty() {
        let what = if variant == "cloud" { " with cloud-init" } else { "" };
        return Err(io::Error::other(format!("no {arch} image{what} for {distro}")));
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

/// How a machine is set apart from the Mac (the .nspawn file's comments).
#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct Isolation {
    files: bool,
    network: bool,
}

const ISOLATED: &str = "# berth: isolated";
const ISOLATED_NETWORK: &str = "# berth: isolate-network";

fn nspawn_path(name: &str) -> String {
    format!("/etc/systemd/nspawn/{name}.nspawn")
}

fn isolation(name: &str) -> Isolation {
    let config = std::fs::read_to_string(nspawn_path(name)).unwrap_or_default();
    Isolation { files: config.lines().any(|l| l == ISOLATED), network: config.lines().any(|l| l == ISOLATED_NETWORK) }
}

/// Folders an isolated machine was given (`Bind=` lines).
fn mounts(name: &str) -> Vec<String> {
    std::fs::read_to_string(nspawn_path(name))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("Bind=").map(str::to_string))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn create(
    name: &str,
    spec: &str,
    user: &str,
    uid: &str,
    home: &str,
    arch: &str,
    user_data: &str,
    isolated: bool,
    isolate_network: bool,
    mounts: &[String],
) -> io::Result<()> {
    if !valid_name(name) {
        return Err(io::Error::other("name: 1-12 characters, a-z 0-9 and -"));
    }
    // Folders for an isolated machine: ones the VM sees (home, /Volumes,
    // /private/tmp...), as plain absolute paths for the .nspawn file.
    for m in mounts {
        if !m.starts_with('/') || m.contains(['\n', ':', '"']) || !Path::new(m).is_dir() {
            return Err(io::Error::other(format!("--mount {m}: not a folder the VM can see (your home, /Volumes, /tmp)")));
        }
    }
    let arch = match arch {
        "" | "arm64" | "aarch64" => "arm64",
        "amd64" | "x86_64" | "x86-64" => "amd64",
        other => return Err(io::Error::other(format!("arch: arm64 or amd64, not {other}"))),
    };
    if arch == "amd64" && !Path::new("/proc/sys/fs/binfmt_misc/rosetta").exists() {
        return Err(io::Error::other("amd64 machines need Rosetta, which isn't set up in the VM"));
    }
    let root = format!("{MACHINES}/{name}");
    if Path::new(&root).exists() {
        return Err(io::Error::other(format!("machine {name} already exists")));
    }
    ensure_tools()?;
    let variant = if user_data.is_empty() { "default" } else { "cloud" };
    let path = image_path(if spec.is_empty() { DEFAULT_DISTRO } else { spec }, arch, variant)?;
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
         echo {name} > /etc/hostname; \
         grep -qw {name} /etc/hosts 2>/dev/null || echo '127.0.1.1 {name}' >> /etc/hosts"
    );
    sh(&format!("systemd-nspawn -q -D {root} --pipe /bin/sh -c '{}'", setup.replace('\'', "'\\''")))?;
    if (isolated || isolate_network) && !["usr/lib/systemd/systemd", "lib/systemd/systemd"].iter().any(|p| Path::new(&format!("{root}/{p}")).exists()) {
        let _ = std::fs::remove_dir_all(&root);
        return Err(io::Error::other(
            "isolated machines need a distribution with systemd (not Alpine): berth reaches into them only through it",
        ));
    }
    if !user_data.is_empty() {
        seed_cloud_init(&root, name, user_data)?;
    }
    if arch == "amd64" {
        rosetta_friendly_services(&root)?;
    }

    let ve = format!("ve-{name}");
    std::fs::create_dir_all("/etc/systemd/nspawn")?;
    let mut marks = String::new();
    let mut binds = String::new();
    if isolated || isolate_network {
        marks.push_str(&format!("{ISOLATED}\n"));
        for m in mounts {
            binds.push_str(&format!("Bind={m}\n"));
        }
    } else {
        binds.push_str(&format!("Bind={home}\nBindReadOnly={}\n", crate::mac::DIR));
    }
    if isolate_network {
        marks.push_str(&format!("{ISOLATED_NETWORK}\n"));
    }
    std::fs::write(
        nspawn_path(name),
        format!(
            "{marks}[Exec]\nBoot=yes\nPrivateUsers=no\n{}\n[Files]\n{binds}\n[Network]\nVirtualEthernetExtra={ve}:eth0\n",
            kill_signal(name)
        ),
    )?;
    if !(isolated || isolate_network) {
        integrate_files(name);
    }
    allow_forwarding();
    apply_isolation();
    sh(&format!("machinectl enable {name} >/dev/null 2>&1; machinectl start {name}"))?;
    Ok(())
}

/// berth's own chains for network-isolated machines, jumped to first from
/// FORWARD, INPUT and Docker's DOCKER-USER: ahead of the machines' general
/// ACCEPT and the DNAT ACCEPT domains.rs adds, whichever comes later.
const CHAIN: &str = "BERTH-ISOLATE";
const CHAIN_IN: &str = "BERTH-ISOLATE-IN";

fn ipt(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).arg("-w").args(args).output().is_ok_and(|o| o.status.success())
}

/// Rebuild the chains from the machines marked isolate-network: DNS (the
/// Mac's resolver) and the internet only. Traffic to private ranges is
/// dropped where the VM forwards it, and to the VM itself
/// (host.docker.internal's REDIRECT lands there) except DHCP and replies;
/// IPv6 isn't forwarded for them at all. Called at agent start, before
/// such a machine starts (berth-agent isolate, an ExecStartPre), and on
/// create, rename and delete.
pub fn apply_isolation() {
    // The agent and a machine's ExecStartPre can rebuild at once: one at a
    // time, or the rules end up twice.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open("/run/berth-isolate.lock");
    if let Ok(file) = &lock {
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(file), libc::LOCK_EX) };
    }
    let dns = sh("ip -4 route show default | awk '{print $3; exit}'").unwrap_or_default().trim().to_string();
    for cmd in ["iptables", "ip6tables"] {
        for chain in [CHAIN, CHAIN_IN] {
            ipt(cmd, &["-N", chain]);
            ipt(cmd, &["-F", chain]);
        }
        // Rules an earlier berth put straight into Docker's chains.
        for parent in ["DOCKER-USER", "INPUT", "FORWARD"] {
            let rules = Command::new(cmd).args(["-w", "-S", parent]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
            for rule in rules.lines().filter(|l| l.contains("berth-isolate")) {
                let _ = Command::new(cmd).arg("-w").args(rule.replacen("-A ", "-D ", 1).split_whitespace()).output();
            }
        }
    }
    for entry in std::fs::read_dir(MACHINES).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_name(&name) || !isolation(&name).network {
            continue;
        }
        let ve = format!("ve-{name}");
        let add = |cmd: &str, chain: &str, rule: &[&str]| {
            let mut args = vec!["-A", chain, "-i", ve.as_str()];
            args.extend_from_slice(rule);
            ipt(cmd, &args);
        };
        let replies = ["-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"];
        add("iptables", CHAIN, &replies);
        if dns.parse::<std::net::Ipv4Addr>().is_ok() {
            for proto in ["udp", "tcp"] {
                add("iptables", CHAIN, &["-d", &dns, "-p", proto, "--dport", "53", "-j", "ACCEPT"]);
            }
        }
        for range in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "169.254.0.0/16", "198.18.0.0/15"] {
            add("iptables", CHAIN, &["-d", range, "-j", "DROP"]);
        }
        add("iptables", CHAIN_IN, &replies);
        add("iptables", CHAIN_IN, &["-p", "udp", "--dport", "67", "-j", "ACCEPT"]);
        add("iptables", CHAIN_IN, &["-j", "DROP"]);
        add("ip6tables", CHAIN, &["-j", "DROP"]);
        add("ip6tables", CHAIN_IN, &replies);
        add("ip6tables", CHAIN_IN, &["-j", "DROP"]);
    }
    ensure_jumps();
}

/// Put the jumps to our chains first again (Docker inserts its own at the
/// top when it starts, and domains.rs adds a DNAT ACCEPT to DOCKER-USER).
pub fn ensure_jumps() {
    for (cmd, parent, chain) in [
        ("iptables", "FORWARD", CHAIN),
        ("iptables", "INPUT", CHAIN_IN),
        ("ip6tables", "FORWARD", CHAIN),
        ("ip6tables", "INPUT", CHAIN_IN),
        ("iptables", "DOCKER-USER", CHAIN),
    ] {
        let Ok(out) = Command::new(cmd).args(["-w", "-S", parent]).output() else { continue };
        if !out.status.success() {
            continue; // no DOCKER-USER until Docker is up
        }
        let jump = format!("-A {parent} -j {chain}");
        let rules = String::from_utf8_lossy(&out.stdout).into_owned();
        // `-S` lists the policy (or -N) first, then the rules in order.
        if rules.lines().nth(1) == Some(jump.as_str()) {
            continue;
        }
        while ipt(cmd, &["-D", parent, "-j", chain]) {}
        ipt(cmd, &["-I", parent, "1", "-j", chain]);
    }
}

/// `berth-agent isolate <name>`, before a machine starts (an ExecStartPre
/// of systemd-nspawn@): the rules, and refuse to start a network-isolated
/// machine whose rules aren't there.
pub fn guard(name: &str) -> bool {
    apply_isolation();
    if !valid_name(name) || !isolation(name).network {
        return true;
    }
    let ours = Command::new("iptables").args(["-w", "-S", CHAIN]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let first = Command::new("iptables").args(["-w", "-S", "FORWARD"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    ours.contains(&format!("-i ve-{name} ")) && first.lines().nth(1) == Some(format!("-A FORWARD -j {CHAIN}").as_str())
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

/// x86-64 systemd sandboxes many services with a seccomp filter for x86-64
/// syscalls and MemoryDenyWriteExecute, but under Rosetta the kernel sees
/// arm64 syscalls and the translator writes code: those services die
/// (journald, networkd, resolved: no network). A drop-in for every service
/// lifts both.
fn rosetta_friendly_services(root: &str) -> io::Result<()> {
    let dir = format!("{root}/etc/systemd/system/service.d");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        format!("{dir}/90-berth-rosetta.conf"),
        "# berth: this machine runs on Rosetta (see agent/src/machines.rs).\n\
         [Service]\nSystemCallArchitectures=\nSystemCallFilter=\nMemoryDenyWriteExecute=no\n",
    )
}

/// The NoCloud seed cloud-init reads on the machine's first boot.
fn seed_cloud_init(root: &str, name: &str, user_data: &str) -> io::Result<()> {
    let seed = format!("{root}/var/lib/cloud/seed/nocloud");
    std::fs::create_dir_all(&seed)?;
    std::fs::write(format!("{seed}/user-data"), user_data)?;
    std::fs::write(format!("{seed}/meta-data"), format!("instance-id: berth-{name}\nlocal-hostname: {name}\n"))?;
    // cloud-init's fallback matches the NIC by permanent MAC address, which
    // a veth doesn't have: networkd would leave eth0 alone. By name instead.
    std::fs::write(format!("{seed}/network-config"), "version: 2\nethernets:\n  eth0:\n    dhcp4: true\n")?;
    // The images switch it off until something configures it.
    let _ = std::fs::remove_file(format!("{root}/etc/cloud/cloud-init.disabled"));
    // Without a hint, cloud-init in a container may not look for NoCloud.
    std::fs::create_dir_all(format!("{root}/etc/cloud/cloud.cfg.d"))?;
    std::fs::write(format!("{root}/etc/cloud/cloud.cfg.d/90-berth.cfg"), "datasource_list: [ NoCloud, None ]\n")?;
    Ok(())
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
    let Ok(root) = crate::inroot::Root::new(format!("{MACHINES}/{name}")) else { return };
    if let Ok(agent) = std::env::current_exe() {
        // The agent runs from the shared home, which machines see at the same path.
        let agent = agent.to_string_lossy();
        if root.read_link("/usr/local/bin/mac").ok().as_deref() != Some(&*agent) {
            let _ = root.symlink(&agent, "/usr/local/bin/mac");
        }
    }
    let profile = format!(
        "# berth: the Mac's SSH agent, unless the session brought its own.\n\
         [ -n \"$SSH_AUTH_SOCK\" ] || [ ! -S {0}/ssh-agent.sock ] || export SSH_AUTH_SOCK={0}/ssh-agent.sock\n",
        crate::mac::DIR
    );
    let _ = root.write_creating("/etc/profile.d/berth.sh", profile.as_bytes());
}

/// Whether the machine's PID 1 is systemd (its own `systemd-run -M` and
/// `machinectl shell` then work, confined like the machine).
fn runs_systemd(name: &str) -> bool {
    sh(&format!("machinectl show {name} -p Leader --value"))
        .ok()
        .and_then(|leader| std::fs::read_to_string(format!("/proc/{}/comm", leader.trim())).ok())
        .is_some_and(|comm| comm.trim() == "systemd")
}

/// Run a script inside a running machine, as its root. Through the
/// machine's own systemd when it has one: the process then has the
/// machine's limits (capabilities, seccomp, devices). Otherwise nsenter,
/// which runs the machine's binaries as the VM's unconfined root: fine for
/// a machine trusted with the Mac anyway, never for an isolated one.
fn run_in(name: &str, script: &str) -> io::Result<String> {
    let quoted = script.replace('\'', "'\\''");
    if runs_systemd(name) {
        return sh(&format!("systemd-run -M {name} --wait --pipe --quiet --collect /bin/sh -c '{quoted}'"));
    }
    if isolation(name).files {
        return Err(io::Error::other("an isolated machine without systemd: berth won't run anything in it"));
    }
    sh(&format!("nsenter -t \"$(machinectl show {name} -p Leader --value)\" -a -- /bin/sh -c '{quoted}'"))
}

/// Bring machines made before an integration feature up to date: files,
/// the /run/berth bind in their .nspawn file and, if running, live.
pub fn integrate_all() {
    let Ok(entries) = std::fs::read_dir(MACHINES) else { return };
    // iptables rules don't survive a reboot; Docker makes DOCKER-USER once
    // it's up, which may be a while on boot.
    std::thread::spawn(|| {
        // Up to 30 min: a first boot installs Docker first.
        for _ in 0..900 {
            if Command::new("iptables").args(["-w", "-L", "DOCKER-USER", "-n"]).output().is_ok_and(|o| o.status.success()) {
                allow_forwarding();
                // After the general ACCEPT, so they come first.
                ensure_jumps();
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !valid_name(&name) || !entry.path().is_dir() || isolation(&name).files {
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
    let user = crate::inroot::Root::new(&root)
        .and_then(|r| r.read("/etc/sudoers.d/berth", 4096))
        .ok()
        .and_then(|s| String::from_utf8_lossy(&s).split_whitespace().next().map(str::to_string))
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
    // A machine that just booted may not have its network yet.
    let mut result = run_in(name, &script);
    for _ in 0..3 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
        result = run_in(name, &script);
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
        "create" => {
            let mounts: Vec<String> = arg(9).lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
            create(arg(0), arg(1), arg(2), arg(3), arg(4), arg(5), arg(6), arg(7) == "1", arg(8) == "1", &mounts).map(|_| "ok".into())
        }
        "restart" => stop(arg(0)).and_then(|_| start(arg(0))).map(|_| "ok".into()),
        "rename" => rename(arg(0), arg(1)).map(|_| "ok".into()),
        "info" => info(arg(0)),
        "logs" => logs(arg(0), arg(1).parse().unwrap_or(100)),
        "copy-to" | "copy-from" => copy(action, arg(0), arg(1), arg(2), arg(3)).map(|_| "ok".into()),
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
                let _ = std::fs::remove_file(nspawn_path(name));
                apply_isolation(); // without it now
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

fn exists(name: &str) -> io::Result<()> {
    if !valid_name(name) || !Path::new(&format!("{MACHINES}/{name}")).is_dir() {
        return Err(io::Error::other(format!("no machine {name}")));
    }
    Ok(())
}

fn running(name: &str) -> bool {
    sh(&format!("machinectl show {name} -p State --value 2>/dev/null")).is_ok_and(|s| s.trim() == "running")
}

/// Rename a machine: its folder, unit, .nspawn file (and its interface
/// name), hostname. Started again if it was running.
fn rename(old: &str, new: &str) -> io::Result<()> {
    exists(old)?;
    if !valid_name(new) {
        return Err(io::Error::other("new name: 1-12 characters, a-z 0-9 and -"));
    }
    if Path::new(&format!("{MACHINES}/{new}")).exists() {
        return Err(io::Error::other(format!("machine {new} already exists")));
    }
    let was_running = running(old);
    if was_running {
        stop(old)?;
    }
    let _ = sh(&format!("machinectl disable {old} >/dev/null 2>&1"));
    std::fs::rename(format!("{MACHINES}/{old}"), format!("{MACHINES}/{new}"))?;
    let config = std::fs::read_to_string(nspawn_path(old)).unwrap_or_default();
    std::fs::write(nspawn_path(new), config.replace(&format!("ve-{old}:"), &format!("ve-{new}:")))?;
    let _ = std::fs::remove_file(nspawn_path(old));
    if let Ok(root) = crate::inroot::Root::new(format!("{MACHINES}/{new}")) {
        let _ = root.write("/etc/hostname", format!("{new}\n").as_bytes());
        if let Ok(hosts) = root.read("/etc/hosts", 1 << 20) {
            let renamed: Vec<String> = String::from_utf8_lossy(&hosts)
                .lines()
                .map(|l| if l.starts_with("127.0.1.1") { format!("127.0.1.1 {new}") } else { l.to_string() })
                .collect();
            let _ = root.write("/etc/hosts", (renamed.join("\n") + "\n").as_bytes());
        }
    }
    apply_isolation();
    sh(&format!("machinectl enable {new} >/dev/null 2>&1"))?;
    if was_running {
        start(new)?;
    }
    Ok(())
}

/// `berth machine info`, as JSON.
fn info(name: &str) -> io::Result<String> {
    exists(name)?;
    let root = format!("{MACHINES}/{name}");
    let machine = list()?.into_iter().find(|m| m.name == name);
    // e_machine of the machine's shell: 0xB7 arm64, 0x3E x86-64 (its
    // symlinks, like Alpine's /bin/sh -> /bin/busybox, resolve inside it).
    let arch = crate::inroot::Root::new(&root)
        .and_then(|r| r.read("/bin/sh", 20))
        .ok()
        .and_then(|elf| elf.get(18).copied())
        .map_or("unknown", |m| match m {
            0xB7 => "arm64",
            0x3E => "amd64 (Rosetta)",
            _ => "unknown",
        });
    let disk = sh(&format!("du -sxh {root} 2>/dev/null | cut -f1")).unwrap_or_default();
    let iso = isolation(name);
    let json = serde_json::json!({
        "name": name,
        "state": machine.as_ref().map(|m| m.state.clone()),
        "os": machine.as_ref().map(|m| m.os.clone()),
        "addresses": machine.as_ref().map(|m| m.addresses.clone()),
        "arch": arch,
        "disk": disk.trim(),
        "isolated": iso.files || iso.network,
        "isolated_network": iso.network,
        "mounts": if iso.files { mounts(name) } else { vec!["home folder".to_string()] },
        "autostart": Path::new(&format!("/etc/systemd/system/machines.target.wants/systemd-nspawn@{name}.service")).exists(),
    });
    serde_json::to_string(&json).map_err(io::Error::other)
}

/// The machine's journal (or syslog where there's no systemd: Alpine).
fn logs(name: &str, lines: u32) -> io::Result<String> {
    exists(name)?;
    let root = format!("{MACHINES}/{name}");
    // nspawn makes journal folders even where nothing writes them (Alpine).
    let journal = format!("{root}/var/log/journal");
    if sh(&format!("find {journal} -name '*.journal' 2>/dev/null | head -1")).is_ok_and(|f| !f.trim().is_empty()) {
        return sh(&format!("journalctl -D {journal} -n {lines} --no-pager 2>&1"));
    }
    if Path::new(&format!("{root}/var/log/messages")).exists() {
        return sh(&format!("tail -n {lines} {root}/var/log/messages"));
    }
    if running(name) {
        return sh(&format!("journalctl -M {name} -n {lines} --no-pager 2>&1"));
    }
    Ok("no logs found (start the machine, or look in /var/log inside it)\n".into())
}

/// Copy between the VM (the Mac's shared folders) and a running machine;
/// files copied in belong to the machine's user (`owner`, "uid:gid").
fn copy(action: &str, name: &str, from: &str, to: &str, owner: &str) -> io::Result<()> {
    exists(name)?;
    for p in [from, to] {
        if !p.starts_with('/') || p.contains(['\n', '\'']) {
            return Err(io::Error::other(format!("{p}: an absolute path, please")));
        }
    }
    if !running(name) {
        start(name)?;
    }
    sh(&format!("machinectl {action} {name} '{from}' '{to}'")).map_err(|e| {
        if e.to_string().contains("File exists") { io::Error::other(format!("{to} already exists; give another name, or remove it first")) } else { e }
    })?;
    if action == "copy-to" && owner.bytes().all(|b| b.is_ascii_digit() || b == b':') && !owner.is_empty() {
        let _ = run_in(name, &format!("chown -R {owner} '{to}'"));
    }
    Ok(())
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
