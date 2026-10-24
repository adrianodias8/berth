//! Kubernetes: a single-node k3s cluster that uses the guest's Docker as its
//! container runtime, so images built with `docker build` run in pods without
//! a registry. Driven by the host over the `k8s <action>` agent target.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;

const VERSION: &str = "v1.37.1+k3s1";
const SHA256: &str = "a1561ca4aef8b99f5588a840d4468ac39c8c1cd92470bbcb9634823fa221741a";
const BIN: &str = "/usr/local/bin/k3s";
const UNIT: &str = "/etc/systemd/system/k3s.service";
pub const KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
/// Pod and service networks, which the host routes to us when enabled.
pub const SUBNETS: [&str; 2] = ["10.42.0.0/16", "10.43.0.0/16"];

const SERVICE: &str = "[Unit]
Description=k3s (berth Kubernetes)
After=docker.service network-online.target
Wants=docker.service

[Service]
Type=notify
ExecStart=/usr/local/bin/k3s server --docker --disable traefik --write-kubeconfig-mode 0600 --tls-san 127.0.0.1
KillMode=process
Delegate=yes
LimitNOFILE=1048576
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
";

/// Services with a cluster IP, as (name, namespace, IP), for their
/// `<name>.<namespace>.svc.cluster.local` domains (domains.rs). The Mac
/// routes the service network here, where kube-proxy takes over.
pub fn services() -> Vec<(String, String, std::net::Ipv4Addr)> {
    if !active() {
        return Vec::new();
    }
    let Ok(json) = sh(&format!("KUBECONFIG={KUBECONFIG} {BIN} kubectl get services -A -o json 2>/dev/null")) else {
        return Vec::new();
    };
    let Ok(list) = serde_json::from_str::<serde_json::Value>(&json) else { return Vec::new() };
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let ip = s["spec"]["clusterIP"].as_str()?.parse().ok()?; // not headless ("None")
            Some((s["metadata"]["name"].as_str()?.to_string(), s["metadata"]["namespace"].as_str()?.to_string(), ip))
        })
        .collect()
}

/// Services change without Docker events: ask for a domain sync when the
/// list does.
pub fn watch_services() {
    let mut last = Vec::new();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let now = services();
        if now != last {
            last = now;
            crate::domains::request_sync();
        }
    }
}

pub fn active() -> bool {
    Command::new("systemctl").args(["is-active", "--quiet", "k3s"]).status().is_ok_and(|s| s.success())
}

fn sh(script: &str) -> io::Result<String> {
    let out = Command::new("sh").args(["-c", script]).output()?;
    if !out.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn install() -> io::Result<()> {
    if std::fs::metadata(BIN).is_ok() && sh(&format!("{BIN} --version")).is_ok_and(|v| v.contains(VERSION)) {
        return Ok(());
    }
    let url = format!(
        "https://github.com/k3s-io/k3s/releases/download/{}/k3s-arm64",
        VERSION.replace('+', "%2B")
    );
    sh(&format!(
        "set -e; curl -fsSL -o {BIN}.tmp '{url}'; echo '{SHA256}  {BIN}.tmp' | sha256sum -c - >/dev/null; \
         chmod 755 {BIN}.tmp; mv {BIN}.tmp {BIN}"
    ))
    .map(|_| ())
}

/// `k8s enable|disable|status|kubeconfig`; replies with one line (or the
/// kubeconfig) and closes.
pub fn handle(action: &str, mut host: UnixStream) -> io::Result<()> {
    let reply = match action {
        "enable" => {
            let result = install().and_then(|_| {
                std::fs::write(UNIT, SERVICE)?;
                sh("systemctl daemon-reload && systemctl enable --now k3s").map(|_| ())
            });
            match result {
                Ok(()) => "ok".to_string(),
                Err(err) => format!("error: {err}"),
            }
        }
        "disable" => match sh("systemctl disable --now k3s; /usr/local/bin/k3s-killall.sh 2>/dev/null; \
                               docker ps -aq --filter name=k8s_ | xargs -r docker rm -f >/dev/null; true") {
            Ok(_) => "ok".to_string(),
            Err(err) => format!("error: {err}"),
        },
        "status" => {
            let enabled = Command::new("systemctl").args(["is-enabled", "--quiet", "k3s"]).status().is_ok_and(|s| s.success());
            let ready = active() && std::fs::metadata(KUBECONFIG).is_ok();
            match (enabled, ready) {
                (_, true) => "running",
                (true, false) => "starting",
                (false, _) => "disabled",
            }
            .to_string()
        }
        "kubeconfig" => std::fs::read_to_string(KUBECONFIG).unwrap_or_default(),
        _ => format!("error: unknown action {action}"),
    };
    host.write_all(reply.as_bytes())
}
