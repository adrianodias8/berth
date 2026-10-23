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

pub async fn sync() {
    let wanted = match network_info().await {
        Ok(NetworkInfo { gateway: Some(gw), subnets }) => {
            subnets.into_iter().map(|subnet| Route { subnet, gateway: gw.clone() }).collect::<Vec<_>>()
        }
        Ok(_) => return,
        Err(err) => {
            eprintln!("routes: {err:#}");
            return;
        }
    };
    if *LAST.lock().unwrap() == wanted && STATE.lock().unwrap().is_some() {
        return;
    }
    let result = tokio::task::spawn_blocking({
        let wanted = wanted.clone();
        move || helper::set_routes(wanted)
    })
    .await;
    let state = match result {
        Ok(None) => return,
        Ok(Some(Ok(reply))) => {
            for err in &reply.errors {
                eprintln!("routes: {err}");
            }
            let active: Vec<&str> = reply.active.iter().map(|r| r.subnet.as_str()).collect();
            if !reply.active.is_empty() {
                eprintln!("routes: active {}", active.join(", "));
            }
            match (active.is_empty(), reply.errors.is_empty()) {
                (_, true) => format!("on ({})", active.join(", ")),
                (true, false) => format!("blocked: {}", reply.errors.join("; ")),
                (false, false) => format!("partial ({}); {}", active.join(", "), reply.errors.join("; ")),
            }
        }
        Ok(Some(Err(err))) => format!("helper error: {err:#}"),
        Err(err) => format!("helper error: {err}"),
    };
    *LAST.lock().unwrap() = wanted;
    *STATE.lock().unwrap() = Some(state);
}

/// Withdraw all routes; called on shutdown so none outlive the VM.
pub fn clear() {
    if !LAST.lock().unwrap().is_empty() {
        let _ = helper::set_routes(Vec::new());
    }
}

async fn network_info() -> anyhow::Result<NetworkInfo> {
    let mut guest = guest_stream("info network").await?;
    let mut json = String::new();
    guest.read_to_string(&mut json).await?;
    Ok(serde_json::from_str(&json)?)
}
