//! The proxy Docker (pulls, pushes) and apt use in the VM, as the host
//! hands it over (src/proxy.rs): `proxy\n` then JSON until EOF, null for
//! none. Docker reads it at start, so a change restarts dockerd, but only
//! while no container runs: with "auto" the proxy changes with the Mac's
//! network, and that mustn't stop anyone's containers. Otherwise it waits
//! until none runs (the host asks again every 15 s) or Docker next starts.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;

use serde::Deserialize;

const DOCKER_DROPIN: &str = "/etc/systemd/system/docker.service.d/berth-proxy.conf";
const APT_CONF: &str = "/etc/apt/apt.conf.d/90berth-proxy";

/// Written but not yet in the running dockerd.
static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
                    env.push_str(&format!("Environment=\"{name}={v}\"\n"));
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
    use std::sync::atomic::Ordering::SeqCst;
    let changed = write_or_remove(DOCKER_DROPIN, dropin.as_deref())?;
    write_or_remove(APT_CONF, apt.as_deref())?;
    if changed {
        PENDING.store(true, SeqCst);
    }
    let reply = if !PENDING.load(SeqCst) {
        "same".to_string()
    } else if running_containers() {
        if changed { "changed; applies once no container runs, or when Docker next starts" } else { "same" }.to_string()
    } else {
        PENDING.store(false, SeqCst);
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
