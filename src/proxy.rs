//! The HTTP(S) proxy Docker uses in the VM for pulls and pushes (and apt
//! for the VM's own packages): `"proxy"` in config.json, "auto" (default)
//! for the Mac's system settings, "none", or a URL. The agent applies it
//! (agent/src/proxy.rs). With "auto" it's checked again every few seconds,
//! since it changes with networks and VPNs.

use serde::Serialize;

use crate::paths::Paths;

/// Where the VM reaches the Mac's loopback (agent/src/hostnet.rs).
const MAC_FROM_VM: &str = "198.19.248.254";

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct Proxy {
    pub http: Option<String>,
    pub https: Option<String>,
    pub no_proxy: String,
}

fn setting(paths: &Paths) -> String {
    #[derive(serde::Deserialize, Default)]
    struct Config {
        proxy: Option<String>,
    }
    std::fs::read(paths.config())
        .ok()
        .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
        .and_then(|c| c.proxy)
        .unwrap_or_else(|| "auto".into())
}

/// What the VM should use now; None for no proxy.
pub fn effective(paths: &Paths) -> Option<Proxy> {
    let setting = setting(paths);
    let (http, https, exceptions) = match setting.as_str() {
        "none" => return None,
        "auto" => system()?,
        url => (Some(url.to_string()), Some(url.to_string()), Vec::new()),
    };
    if http.is_none() && https.is_none() {
        return None;
    }
    // berth's own names and networks never go through it.
    let mut no_proxy = vec![
        "localhost".to_string(),
        "127.0.0.1".into(),
        "::1".into(),
        "host.docker.internal".into(),
        ".local".into(),
        "10.0.0.0/8".into(),
        "172.16.0.0/12".into(),
        "192.168.0.0/16".into(),
        MAC_FROM_VM.into(),
    ];
    no_proxy.extend(exceptions);
    Some(Proxy { http: http.map(|u| for_vm(&u)), https: https.map(|u| for_vm(&u)), no_proxy: no_proxy.join(",") })
}

/// A proxy on the Mac's loopback is reached from the VM through
/// host.docker.internal's address.
fn for_vm(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((auth, hp)) => (format!("{auth}@"), hp),
        None => (String::new(), rest),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.bytes().all(|b| b.is_ascii_digit() || b == b'/') => (h, format!(":{p}")),
        _ => (hostport, String::new()),
    };
    let host = match host.trim_matches(['[', ']']) {
        "127.0.0.1" | "localhost" | "::1" => MAC_FROM_VM,
        _ => host,
    };
    format!("{scheme}://{auth}{host}{port}")
}

/// The Mac's HTTP and HTTPS proxies and exceptions (`scutil --proxy`).
/// Auto-config (PAC) files aren't followed.
fn system() -> Option<(Option<String>, Option<String>, Vec<String>)> {
    let out = std::process::Command::new("/usr/sbin/scutil").arg("--proxy").output().ok()?;
    Some(parse_scutil(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_scutil(text: &str) -> (Option<String>, Option<String>, Vec<String>) {
    let mut values = std::collections::HashMap::new();
    let mut exceptions = Vec::new();
    let mut in_exceptions = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with("ExceptionsList") {
            in_exceptions = true;
            continue;
        }
        if in_exceptions {
            if line == "}" {
                in_exceptions = false;
            } else if let Some((_, v)) = line.split_once(" : ") {
                exceptions.push(cidr(v.trim()));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(" : ") {
            values.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let proxy = |kind: &str| {
        (values.get(&format!("{kind}Enable")).map(String::as_str) == Some("1")).then(|| {
            let host = values.get(&format!("{kind}Proxy")).cloned().unwrap_or_default();
            let port = values.get(&format!("{kind}Port")).cloned().unwrap_or_else(|| "80".into());
            format!("http://{host}:{port}")
        })
    };
    // "*.example.com" means the domain and below, as NO_PROXY's ".example.com".
    let exceptions = exceptions.into_iter().map(|e| e.strip_prefix('*').map(str::to_string).unwrap_or(e)).collect();
    (proxy("HTTP"), proxy("HTTPS"), exceptions)
}

/// macOS writes networks as "169.254/16"; NO_PROXY wants 169.254.0.0/16.
fn cidr(entry: &str) -> String {
    let Some((net, bits)) = entry.split_once('/') else { return entry.to_string() };
    let parts: Vec<&str> = net.split('.').collect();
    if parts.len() < 4 && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
        let mut full = parts.clone();
        full.resize(4, "0");
        return format!("{}/{bits}", full.join("."));
    }
    entry.to_string()
}

/// Hand the proxy to the agent; it only acts on a change.
pub async fn push(paths: &Paths) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let proxy = effective(paths);
    let Ok(json) = serde_json::to_string(&proxy) else { return };
    // In the body: exceptions can make it longer than a header may be.
    if let Ok(mut guest) = crate::daemon::guest_stream("proxy").await
        && guest.write_all(json.as_bytes()).await.is_ok()
        && guest.shutdown().await.is_ok()
    {
        let mut reply = String::new();
        let _ = guest.read_to_string(&mut reply).await;
        if reply.starts_with("changed") {
            eprintln!("proxy: {}", reply.trim());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_proxies_are_reached_through_the_mac_address() {
        assert_eq!(for_vm("http://127.0.0.1:3128"), "http://198.19.248.254:3128");
        assert_eq!(for_vm("http://user:pw@localhost:8080"), "http://user:pw@198.19.248.254:8080");
        assert_eq!(for_vm("http://proxy.corp:3128"), "http://proxy.corp:3128");
        assert_eq!(for_vm("http://[::1]:9000"), "http://198.19.248.254:9000");
    }

    #[test]
    fn system_settings() {
        let text = "<dictionary> {\n  ExceptionsList : <array> {\n    0 : *.local\n    1 : 169.254/16\n  }\n  HTTPEnable : 1\n  HTTPPort : 3128\n  HTTPProxy : 127.0.0.1\n  HTTPSEnable : 0\n}\n";
        let (http, https, exceptions) = parse_scutil(text);
        assert_eq!(http.as_deref(), Some("http://127.0.0.1:3128"));
        assert_eq!(https, None);
        assert_eq!(exceptions, [".local", "169.254.0.0/16"]);
        assert_eq!(parse_scutil("<dictionary> {\n  FTPPassive : 1\n}\n"), (None, None, vec![]));
    }
}
