//! Alternative container engines next to Docker: Podman (Debian's package,
//! API socket bridged to the Mac) and nerdctl (on the containerd that Docker
//! already runs, in its own namespace, with CNI plugins and BuildKit for
//! builds). Driven by the host over `engine <action> <name>`.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;

use serde::Serialize;

pub const PODMAN_SOCKET: &str = "/run/podman/podman.sock";

const NERDCTL_VERSION: &str = "2.4.1";
const NERDCTL_SHA256: &str = "2b60e7311589342abec39ffd2c172bef51da31ccc1278a89ec3aa04d1d3040bb";
const BUILDKIT_VERSION: &str = "v0.34.1";
const BUILDKIT_SHA256: &str = "c8c97b84baf89dfe0ea85063ac24ad88eb909979ea476ce3ef9e8e11e60ebef8";

const NERDCTL_TOML: &str = "# berth: use the containerd Docker runs, in its own namespace.
address = \"/run/containerd/containerd.sock\"
namespace = \"default\"
cni_path = \"/usr/lib/cni\"
";

const BUILDKIT_UNIT: &str = "[Unit]
Description=BuildKit (for nerdctl build)
After=containerd.service
Requires=containerd.service

[Service]
ExecStart=/usr/local/bin/buildkitd --oci-worker=false --containerd-worker=true --containerd-worker-namespace=default
Restart=always

[Install]
WantedBy=multi-user.target
";

fn sh(script: &str) -> io::Result<String> {
    let out = Command::new("sh").args(["-c", script]).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(err.lines().last().unwrap_or("command failed").to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn enabled(unit: &str) -> bool {
    Command::new("systemctl").args(["is-active", "--quiet", unit]).status().is_ok_and(|s| s.success())
}

pub fn podman_enabled() -> bool {
    enabled("podman.socket")
}

/// Containers use berth's DNS (for *.berth.local and the Mac's resolvers),
/// and Podman's own host.containers.internal / host.docker.internal point at
/// the Mac rather than the VM (see hostnet.rs).
const PODMAN_CONF: &str = "# berth
[containers]
dns_servers = [\"198.19.248.253\"]
host_containers_internal_ip = \"198.19.248.254\"
";

fn enable_podman() -> io::Result<()> {
    if !std::path::Path::new("/usr/bin/podman").exists() {
        sh("DEBIAN_FRONTEND=noninteractive apt-get install -y -q podman >/dev/null")?;
    }
    std::fs::create_dir_all("/etc/containers/containers.conf.d")?;
    std::fs::write("/etc/containers/containers.conf.d/berth.conf", PODMAN_CONF)?;
    // The API service reads containers.conf at start: restart it if running
    // (the socket starts it again on the next request).
    sh("systemctl enable --now podman.socket && systemctl stop podman.service").map(|_| ())
}

fn enable_nerdctl() -> io::Result<()> {
    if !std::path::Path::new("/usr/local/bin/nerdctl").exists() {
        sh(&format!(
            "set -e; cd /tmp; \
             curl -fsSL -o nerdctl.tgz https://github.com/containerd/nerdctl/releases/download/v{NERDCTL_VERSION}/nerdctl-{NERDCTL_VERSION}-linux-arm64.tar.gz; \
             echo '{NERDCTL_SHA256}  nerdctl.tgz' | sha256sum -c - >/dev/null; \
             tar -xzf nerdctl.tgz -C /usr/local/bin nerdctl; rm nerdctl.tgz"
        ))?;
    }
    if !std::path::Path::new("/usr/local/bin/buildkitd").exists() {
        sh(&format!(
            "set -e; cd /tmp; \
             curl -fsSL -o buildkit.tgz https://github.com/moby/buildkit/releases/download/{BUILDKIT_VERSION}/buildkit-{BUILDKIT_VERSION}.linux-arm64.tar.gz; \
             echo '{BUILDKIT_SHA256}  buildkit.tgz' | sha256sum -c - >/dev/null; \
             tar -xzf buildkit.tgz -C /usr/local bin/buildkitd bin/buildctl; rm buildkit.tgz"
        ))?;
    }
    if !std::path::Path::new("/usr/lib/cni/bridge").exists() {
        sh("DEBIAN_FRONTEND=noninteractive apt-get install -y -q containernetworking-plugins >/dev/null")?;
    }
    std::fs::create_dir_all("/etc/nerdctl")?;
    std::fs::write("/etc/nerdctl/nerdctl.toml", NERDCTL_TOML)?;
    std::fs::write("/etc/systemd/system/buildkit.service", BUILDKIT_UNIT)?;
    sh("systemctl daemon-reload && systemctl enable --now buildkit").map(|_| ())
}

pub fn nerdctl_enabled() -> bool {
    std::path::Path::new("/usr/local/bin/nerdctl").exists()
}

fn nerdctl_json(args: &[&str]) -> Vec<serde_json::Value> {
    Command::new("/usr/local/bin/nerdctl")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or_default()
}

/// Running nerdctl containers in the shape of Docker's `/containers/json`, so
/// domains, routes and port forwarding treat them like Docker's. nerdctl has
/// no API socket, but its `inspect` output is Docker-compatible.
pub fn nerdctl_containers() -> Vec<serde_json::Value> {
    if !nerdctl_enabled() {
        return Vec::new();
    }
    let ids = sh("/usr/local/bin/nerdctl ps -q").unwrap_or_default();
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["inspect"];
    args.extend(ids);
    nerdctl_json(&args).iter().map(as_list_entry).collect()
}

fn as_list_entry(d: &serde_json::Value) -> serde_json::Value {
    let ns = &d["NetworkSettings"];
    let mut ports = Vec::new();
    if let Some(map) = ns["Ports"].as_object() {
        for (key, binds) in map {
            let (private, proto) = key.split_once('/').unwrap_or((key, "tcp"));
            for bind in binds.as_array().into_iter().flatten() {
                ports.push(serde_json::json!({
                    "IP": bind["HostIp"],
                    "PrivatePort": private.parse::<u16>().unwrap_or(0),
                    "PublicPort": bind["HostPort"].as_str().and_then(|p| p.parse::<u16>().ok()),
                    "Type": proto,
                }));
            }
        }
    }
    serde_json::json!({
        "Id": d["Id"],
        "Names": [format!("/{}", d["Name"].as_str().unwrap_or_default())],
        "Labels": if d["Config"]["Labels"].is_object() { d["Config"]["Labels"].clone() } else { serde_json::json!({}) },
        "NetworkSettings": { "Networks": ns["Networks"] },
        "Ports": ports,
        "Mounts": d["Mounts"],
    })
}

/// IPv4 subnets of nerdctl's networks, for routing container IPs.
pub fn nerdctl_subnets() -> Vec<String> {
    if !nerdctl_enabled() {
        return Vec::new();
    }
    let names = sh("/usr/local/bin/nerdctl network ls --format '{{.Name}}'").unwrap_or_default();
    let mut args = vec!["network", "inspect"];
    args.extend(names.split_whitespace().filter(|n| *n != "host" && *n != "none"));
    nerdctl_json(&args)
        .iter()
        .flat_map(|n| n["IPAM"]["Config"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["Subnet"].as_str().map(str::to_string))
        .filter(|s| !s.contains(':'))
        .collect()
}

/// `nerdctl inspect` for a single container (Docker-compatible JSON).
pub fn nerdctl_inspect(id: &str) -> Option<serde_json::Value> {
    nerdctl_json(&["inspect", id]).into_iter().next()
}

/// Follow nerdctl's events: re-sync domains here and port forwarding on the
/// host whenever containers change.
pub fn watch_nerdctl_events() {
    use std::io::BufRead;
    loop {
        if nerdctl_enabled()
            && let Ok(mut child) = Command::new("/usr/local/bin/nerdctl")
                .args(["events", "--format", "{{json .}}"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
        {
            let stdout = child.stdout.take().expect("piped");
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.contains("/tasks/start") || line.contains("/tasks/exit") || line.contains("/containers/delete") {
                    crate::domains::request_sync();
                    notify_host_sync();
                }
            }
            let _ = child.wait();
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

fn notify_host_sync() {
    use crate::hostnet::{HOST_VSOCK_PORT, VMADDR_CID_HOST, vsock_connect};
    if let Ok(fd) = vsock_connect(VMADDR_CID_HOST, HOST_VSOCK_PORT) {
        let _ = UnixStream::from(fd).write_all(b"sync\n");
    }
}

#[derive(Serialize)]
struct Status {
    podman: bool,
    nerdctl: bool,
}

/// `engine enable|disable <podman|nerdctl>` or `engine status`.
pub fn handle(args: &str, mut host: UnixStream) -> io::Result<()> {
    let (action, name) = args.split_once(' ').unwrap_or((args, ""));
    let result: io::Result<String> = match (action, name) {
        ("enable", "podman") => enable_podman().map(|_| "ok".into()),
        ("enable", "nerdctl") => enable_nerdctl().map(|_| "ok".into()),
        ("disable", "podman") => sh("systemctl disable --now podman.socket podman.service").map(|_| "ok".into()),
        ("disable", "nerdctl") => sh("systemctl disable --now buildkit").map(|_| "ok".into()),
        ("status", _) => serde_json::to_string(&Status {
            podman: podman_enabled(),
            nerdctl: std::path::Path::new("/usr/local/bin/nerdctl").exists() && enabled("buildkit"),
        })
        .map_err(io::Error::other),
        _ => Err(io::Error::other(format!("unknown engine command: {args}"))),
    };
    if result.is_ok() && action != "status" {
        // New engine, new containers/networks to track.
        crate::domains::request_sync();
    }
    host.write_all(result.unwrap_or_else(|e| format!("error: {e}")).as_bytes())
}
