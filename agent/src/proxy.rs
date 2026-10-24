//! The proxy Docker (pulls, pushes) and apt use in the VM, as the host
//! hands it over (src/proxy.rs): `proxy\n` then JSON until EOF, null for
//! none. Docker reads it at start, and a restart would kill builds, pulls
//! and containers in progress (with "auto" the proxy changes with the Mac's
//! network), so a change applies when Docker next starts. Except just after
//! the VM booted, before anything runs: then Docker is restarted once, so a
//! proxy that changed while the VM was off applies now.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;

use serde::Deserialize;

const DOCKER_DROPIN: &str = "/etc/systemd/system/docker.service.d/berth-proxy.conf";
const APT_CONF: &str = "/etc/apt/apt.conf.d/90berth-proxy";

/// How long after the VM booted Docker may still be restarted.
const BOOT_WINDOW_SECS: f64 = 120.0;

fn just_booted() -> bool {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .is_some_and(|up| up < BOOT_WINDOW_SECS)
}

#[derive(Deserialize)]
struct Proxy {
    http: Option<String>,
    https: Option<String>,
    no_proxy: String,
}

/// Values go into systemd and apt syntax: no quotes, newlines or spaces.
fn clean(s: &str) -> Option<String> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\' | b'\'' | b';'))).then(|| s.to_string())
}

pub fn handle(mut host: UnixStream) -> io::Result<()> {
    let mut body = String::new();
    (&mut host).take(64 * 1024).read_to_string(&mut body)?;
    let proxy: Option<Proxy> = serde_json::from_str(&body).map_err(io::Error::other)?;
    let (dropin, apt) = match &proxy {
        None => (None, None),
        Some(p) => {
            let http = p.http.as_deref().and_then(clean);
            let https = p.https.as_deref().and_then(clean);
            let no_proxy = clean(&p.no_proxy).unwrap_or_default();
            let mut env = String::from("# Written by berth (proxy setting).\n[Service]\n");
            let mut apt = String::from("// Written by berth (proxy setting).\n");
            for (name, value) in [("HTTP_PROXY", &http), ("HTTPS_PROXY", &https)] {
                if let Some(v) = value {
                    // systemd expands %-specifiers: a URL-encoded password has %.
                    env.push_str(&format!("Environment=\"{name}={}\"\n", v.replace('%', "%%")));
                }
            }
            env.push_str(&format!("Environment=\"NO_PROXY={no_proxy}\"\n"));
            if let Some(v) = &http {
                apt.push_str(&format!("Acquire::http::Proxy \"{v}\";\n"));
            }
            if let Some(v) = https.as_ref().or(http.as_ref()) {
                apt.push_str(&format!("Acquire::https::Proxy \"{v}\";\n"));
            }
            (Some(env), Some(apt))
        }
    };
    let changed = write_or_remove(DOCKER_DROPIN, dropin.as_deref())?;
    write_or_remove(APT_CONF, apt.as_deref())?;
    let booting = just_booted();
    let reply = if !changed {
        "same".to_string()
    } else if !booting || running_containers() {
        "changed; applies when Docker next starts (berth stop --shutdown && berth start)".to_string()
    } else {
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        let _ = Command::new("systemctl").args(["restart", "docker"]).status();
        format!("changed; Docker restarted with {}", proxy.as_ref().and_then(|p| p.https.as_ref().or(p.http.as_ref())).map_or("no proxy", |s| s))
    };
    if reply.starts_with("changed") {
        // Picked up by the next start if not now.
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        eprintln!("berth-agent: proxy: {reply}");
    }
    host.write_all(reply.as_bytes())
}

/// Returns whether the file changed.
fn write_or_remove(path: &str, contents: Option<&str>) -> io::Result<bool> {
    let current = std::fs::read_to_string(path).ok();
    match contents {
        Some(c) if current.as_deref() != Some(c) => {
            if let Some(dir) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, c)?;
            Ok(true)
        }
        None if current.is_some() => {
            std::fs::remove_file(path)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn running_containers() -> bool {
    crate::docker::containers().map_or(true, |c| !c.is_empty())
}
