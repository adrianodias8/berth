//! Direct container IPs: keep the route helper's routes in step with the
//! guest's Docker subnets, all via the guest's address on the NAT network.

use std::sync::Mutex;

use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::daemon::guest_stream;
use crate::helper::{self, Route};

#[derive(Deserialize)]
struct NetworkInfo {
    gateway: Option<String>,
    subnets: Vec<String>,
}

/// The guest's address on the NAT network, learned on each sync. Port
/// forwarding dials it directly: virtio-net is several times faster than vsock.
pub static GUEST_IP: Mutex<Option<std::net::Ipv4Addr>> = Mutex::new(None);

/// Whether the guest reports Kubernetes' subnets, i.e. k3s is running.
pub static K8S_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Last outcome, for `berth status`.
static STATE: Mutex<Option<String>> = Mutex::new(None);
static LAST: Mutex<Vec<Route>> = Mutex::new(Vec::new());

pub fn state() -> String {
    STATE.lock().unwrap().clone().unwrap_or_else(|| {
        if std::path::Path::new(helper::SOCKET).exists() {
            "pending".into()
        } else {
            "off (enable with: sudo berth helper install)".into()
        }
    })
}

/// Re-assert routes every so often even without Docker events: the helper may
/// have been installed or restarted (dropping its routes) since the last sync.
pub async fn run_periodic() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        sync_with(true).await;
    }
}

pub async fn sync() {
    sync_with(false).await;
}

async fn sync_with(force: bool) {
    let wanted = match network_info().await {
        Ok(NetworkInfo { gateway: Some(gw), subnets }) => {
            *GUEST_IP.lock().unwrap() = gw.parse().ok();
            K8S_ACTIVE.store(subnets.iter().any(|s| s == "10.43.0.0/16"), std::sync::atomic::Ordering::Relaxed);
            subnets.into_iter().map(|subnet| Route { subnet, gateway: gw.clone() }).collect::<Vec<_>>()
        }
        Ok(_) => return,
        Err(err) => {
            eprintln!("routes: {err:#}");
            return;
        }
    };
    if !force && *LAST.lock().unwrap() == wanted && STATE.lock().unwrap().is_some() {
        return;
    }
    let result = tokio::task::spawn_blocking({
        let wanted = wanted.clone();
        // Also (re)point /var/run/docker.sock at us, once per change, as OrbStack
        // does for its socket.
        let paths = crate::paths::Paths::new();
        let socket = paths.docker_socket().to_string_lossy().into_owned();
        let cli = crate::cli_tools::bin_dir(&paths).to_string_lossy().into_owned();
        move || helper::set_routes(wanted, Some(socket), Some(cli))
    })
    .await;
    let state = match result {
        Ok(None) => return,
        Ok(Some(Ok(reply))) if reply.version < helper::VERSION => {
            "helper outdated (reinstall: Berth Settings, or sudo berth helper install)".to_string()
        }
        Ok(Some(Ok(reply))) => {
            let active: Vec<&str> = reply.active.iter().map(|r| r.subnet.as_str()).collect();
            match (active.is_empty(), reply.errors.is_empty()) {
                (_, true) => format!("on ({})", active.join(", ")),
                (true, false) => format!("blocked: {}", reply.errors.join("; ")),
                (false, false) => format!("partial ({}); {}", active.join(", "), reply.errors.join("; ")),
            }
        }
        Ok(Some(Err(err))) => format!("helper error: {err:#}"),
        Err(err) => format!("helper error: {err}"),
    };
    // Periodic re-syncs repeat themselves; only log changes.
    if STATE.lock().unwrap().as_deref() != Some(state.as_str()) {
        eprintln!("routes: {state}");
    }
    *LAST.lock().unwrap() = wanted;
    *STATE.lock().unwrap() = Some(state);
}

/// Withdraw all routes; called on shutdown so none outlive the VM.
pub fn clear() {
    if !LAST.lock().unwrap().is_empty() {
        let _ = helper::set_routes(Vec::new(), None, None);
    }
}

async fn network_info() -> anyhow::Result<NetworkInfo> {
    let mut guest = guest_stream("info network").await?;
    let mut json = String::new();
    guest.read_to_string(&mut json).await?;
    Ok(serde_json::from_str(&json)?)
}
