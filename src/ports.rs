//! Publish container ports on the Mac. Follows the Docker event stream and
//! keeps one host listener per published TCP port, forwarding each connection
//! to the same port inside the guest (where dockerd's proxy picks it up).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio::task::JoinHandle;

use crate::daemon::{DOCKER_TARGET, guest_stream, is_disconnect};

/// Host listen address and port.
type Key = (&'static str, u16);

static FORWARDS: Mutex<BTreeMap<Key, JoinHandle<()>>> = Mutex::new(BTreeMap::new());

/// Events after which published ports or network subnets may have changed:
/// container start/die/pause/unpause and any network event.
const EVENTS_PATH: &str = "/events?filters=%7B%22type%22%3A%5B%22container%22%2C%22network%22%5D%2C%22event%22%3A%5B%22start%22%2C%22die%22%2C%22pause%22%2C%22unpause%22%2C%22create%22%2C%22destroy%22%2C%22connect%22%2C%22disconnect%22%5D%7D";

#[derive(Deserialize)]
struct Container {
    #[serde(rename = "Ports", default)]
    ports: Vec<Port>,
}

#[derive(Deserialize)]
struct Port {
    #[serde(rename = "IP", default)]
    ip: String,
    #[serde(rename = "PublicPort")]
    public: Option<u16>,
    #[serde(rename = "Type")]
    kind: String,
}

/// Human-readable list of active forwards, for `berth status`.
pub fn forwarded() -> Vec<String> {
    FORWARDS
        .lock()
        .unwrap()
        .keys()
        .map(|&key| display(key))
        .collect()
}

fn display((bind, port): Key) -> String {
    match bind {
        "::" => format!("*:{port}"),
        _ => format!("{bind}:{port}"),
    }
}

pub async fn run() {
    let mut synced = false;
    loop {
        if let Err(err) = watch(&mut synced).await {
            // Failures before the first sync are just the guest still booting.
            if synced {
                eprintln!("ports: {err:#}");
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn watch(synced: &mut bool) -> Result<()> {
    // Subscribe before the initial sync so no event in between is missed.
    let mut events = docker_get(EVENTS_PATH).await?.lines();
    sync().await?;
    *synced = true;
    while events.next_line().await?.is_some() {
        sync().await?;
    }
    bail!("docker event stream ended");
}

async fn sync() -> Result<()> {
    crate::routes::sync().await;
    let mut body = String::new();
    docker_get("/containers/json").await?.read_to_string(&mut body).await?;
    let containers: Vec<Container> = serde_json::from_str(&body)?;

    let mut wanted: Vec<Key> = containers
        .iter()
        .flat_map(|c| &c.ports)
        .filter(|p| p.kind == "tcp")
        .filter_map(|p| Some((bind_address(&p.ip)?, p.public?)))
        .collect();
    wanted.sort();
    wanted.dedup();

    let mut forwards = FORWARDS.lock().unwrap();
    forwards.retain(|key, task| {
        let keep = wanted.contains(key);
        if !keep {
            task.abort();
            eprintln!("ports: stopped forwarding {}", display(*key));
        }
        keep
    });
    for key in wanted {
        if forwards.contains_key(&key) {
            continue;
        }
        // Bind synchronously so a conflict is reported now and retried on the
        // next event instead of failing silently in a task.
        let std = match std::net::TcpListener::bind((key.0, key.1)) {
            Ok(l) => l,
            Err(err) => {
                eprintln!("ports: cannot listen on {}: {err}", display(key));
                continue;
            }
        };
        std.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std)?;
        eprintln!("ports: forwarding {}", display(key));
        forwards.insert(key, tokio::spawn(forward(listener, key.1)));
    }
    Ok(())
}

/// Map Docker's host IP for a binding to where we listen on the Mac.
/// `[::]` is dual-stack on macOS, so it covers IPv4 and IPv6 clients.
fn bind_address(ip: &str) -> Option<&'static str> {
    match ip {
        "" | "0.0.0.0" | "::" => Some("::"),
        "127.0.0.1" | "::1" => Some("127.0.0.1"),
        // Bound to a specific guest address; nothing sensible on the host.
        _ => None,
    }
}

async fn forward(listener: TcpListener, port: u16) {
    let target = format!("tcp 127.0.0.1:{port}");
    loop {
        let Ok((mut client, _)) = listener.accept().await else { continue };
        let target = target.clone();
        tokio::spawn(async move {
            let _ = client.set_nodelay(true);
            let result = async {
                let mut guest = guest_stream(&target).await?;
                tokio::io::copy_bidirectional_with_sizes(&mut client, &mut guest, crate::BUF, crate::BUF).await?;
                anyhow::Ok(())
            };
            if let Err(err) = result.await
                && !is_disconnect(&err)
            {
                eprintln!("ports: {port}: {err:#}");
            }
        });
    }
}

/// GET from the guest's Docker API; returns the body reader after the headers.
async fn docker_get(path: &str) -> Result<BufReader<UnixStream>> {
    let mut stream = guest_stream(DOCKER_TARGET).await?;
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: docker\r\n\r\n").as_bytes())
        .await?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).await?;
    if !status.contains(" 200 ") {
        bail!("docker {path}: {}", status.trim());
    }
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 || line == "\r\n" {
            break;
        }
    }
    Ok(reader)
}
