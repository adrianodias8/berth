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

use crate::daemon::{DOCKER_TARGET, PODMAN_TARGET, guest_stream, is_disconnect};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Proto {
    Tcp,
    Udp,
}

/// Host listen address, port and protocol.
type Key = (&'static str, u16, Proto);

static FORWARDS: Mutex<BTreeMap<Key, JoinHandle<()>>> = Mutex::new(BTreeMap::new());

/// Events after which published ports or network subnets may have changed:
/// container start/die/pause/unpause and any network event.
const EVENTS_PATH: &str = "/events?filters=%7B%22type%22%3A%5B%22container%22%2C%22network%22%5D%2C%22event%22%3A%5B%22start%22%2C%22die%22%2C%22pause%22%2C%22unpause%22%2C%22create%22%2C%22destroy%22%2C%22connect%22%2C%22disconnect%22%5D%7D";

#[derive(Deserialize)]
struct Container {
    #[serde(rename = "Ports", default)]
    ports: Vec<Port>,
    #[serde(rename = "Mounts", default)]
    mounts: Vec<Mount>,
}

#[derive(Deserialize)]
struct Mount {
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "Source", default)]
    source: String,
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

fn display((bind, port, proto): Key) -> String {
    let suffix = if proto == Proto::Udp { "/udp" } else { "" };
    match bind {
        "::" => format!("*:{port}{suffix}"),
        _ => format!("{bind}:{port}{suffix}"),
    }
}

pub async fn run() {
    // Podman containers publish ports too; follow its event stream as well
    // (the socket only answers while Podman is enabled).
    tokio::spawn(async {
        loop {
            if let Ok(events) = docker_get_from(PODMAN_TARGET, "/events").await {
                let mut lines = events.lines();
                while let Ok(Some(_)) = lines.next_line().await {
                    let _ = sync().await;
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
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

/// Re-sync now (the agent asks when nerdctl containers change).
pub fn trigger() {
    tokio::spawn(async {
        let _ = sync().await;
    });
}

async fn sync() -> Result<()> {
    crate::routes::sync().await;
    let mut body = String::new();
    docker_get("/containers/json").await?.read_to_string(&mut body).await?;
    let mut containers: Vec<Container> = serde_json::from_str(&body)?;
    // Podman's Docker-compatible list has the same shape; absent if disabled.
    let mut podman = String::new();
    if let Ok(mut reader) = docker_get_from(PODMAN_TARGET, "/containers/json").await
        && reader.read_to_string(&mut podman).await.is_ok()
        && let Ok(more) = serde_json::from_str::<Vec<Container>>(&podman)
    {
        containers.extend(more);
    }
    // nerdctl has no API socket; the agent lists its containers in this shape.
    let mut nerdctl = String::new();
    if let Ok(mut guest) = guest_stream("info nerdctl").await
        && guest.read_to_string(&mut nerdctl).await.is_ok()
        && let Ok(more) = serde_json::from_str::<Vec<Container>>(&nerdctl)
    {
        containers.extend(more);
    }

    // Same event stream drives file-change forwarding for bind mounts.
    let home = crate::paths::Paths::new().home;
    let sources = containers.iter().flat_map(|c| &c.mounts).filter(|m| m.kind == "bind").map(|m| m.source.as_str());
    crate::watch::set_roots(crate::watch::bind_roots(sources, &home));

    let mut wanted: Vec<Key> = containers
        .iter()
        .flat_map(|c| &c.ports)
        .filter_map(|p| {
            let proto = match p.kind.as_str() {
                "tcp" => Proto::Tcp,
                "udp" => Proto::Udp,
                _ => return None,
            };
            let bind = bind_address(&p.ip)?;
            // UDP is relayed to the guest's NAT address, which only reaches
            // ports Docker binds on all guest interfaces.
            if proto == Proto::Udp && bind != "::" {
                return None;
            }
            Some((bind, p.public?, proto))
        })
        .collect();
    // The Kubernetes API, for the `berth` kubectl context.
    if crate::routes::K8S_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
        wanted.push(("127.0.0.1", crate::k8s::API_PORT, Proto::Tcp));
    }
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
        let task = match key.2 {
            Proto::Tcp => bind_tcp(key.0, key.1).and_then(|std| {
                std.set_nonblocking(true)?;
                // Ports Docker binds on all guest interfaces can be reached on
                // the guest's NAT address; loopback-only ones only via the agent.
                Ok(tokio::spawn(forward(TcpListener::from_std(std)?, key.1, key.0 == "::")))
            }),
            Proto::Udp => bind_udp(key.0, key.1).and_then(|std| {
                std.set_nonblocking(true)?;
                Ok(tokio::spawn(forward_udp(tokio::net::UdpSocket::from_std(std)?, key.1)))
            }),
        };
        match task {
            Ok(task) => {
                eprintln!("ports: forwarding {}", display(key));
                forwards.insert(key, task);
            }
            Err(err) => eprintln!("ports: cannot listen on {}: {err}", display(key)),
        }
    }
    Ok(())
}

/// macOS lets users bind ports below 1024 only on all interfaces; for
/// loopback ones (DDEV's router on 127.0.0.1:80/443, say) the root helper
/// binds and hands us the socket.
fn privileged(addr: &str, port: u16, err: &std::io::Error) -> Option<std::net::IpAddr> {
    let ip: std::net::IpAddr = addr.parse().ok()?;
    (err.kind() == std::io::ErrorKind::PermissionDenied && port < 1024 && ip.is_loopback()).then_some(ip)
}

fn bind_tcp(addr: &str, port: u16) -> std::io::Result<std::net::TcpListener> {
    std::net::TcpListener::bind((addr, port)).or_else(|err| {
        let Some(ip) = privileged(addr, port, &err) else { return Err(err) };
        let fd = crate::helper::bind_privileged(ip, port, false).map_err(|e| std::io::Error::other(format!("{err}; helper: {e:#}")))?;
        let listener = std::net::TcpListener::from(fd);
        // The helper only binds; listening is ours.
        if unsafe { libc::listen(std::os::fd::AsRawFd::as_raw_fd(&listener), 1024) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(listener)
    })
}

fn bind_udp(addr: &str, port: u16) -> std::io::Result<std::net::UdpSocket> {
    std::net::UdpSocket::bind((addr, port)).or_else(|err| {
        let Some(ip) = privileged(addr, port, &err) else { return Err(err) };
        let fd = crate::helper::bind_privileged(ip, port, true).map_err(|e| std::io::Error::other(format!("{err}; helper: {e:#}")))?;
        Ok(std::net::UdpSocket::from(fd))
    })
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

/// UDP relay: one upstream socket per client address, connected to the
/// guest's NAT address, dropped after a minute without traffic.
async fn forward_udp(socket: tokio::net::UdpSocket, port: u16) {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;

    const IDLE: Duration = Duration::from_secs(60);
    // Reply tasks hold the listening socket; they must die with this task
    // (dropped on abort when the port goes away), or the port stays bound.
    struct AbortOnDrop(JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    type Session = (Arc<tokio::net::UdpSocket>, Arc<Mutex<std::time::Instant>>, AbortOnDrop);

    let socket = Arc::new(socket);
    let mut sessions: HashMap<SocketAddr, Session> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let Ok((n, client)) = socket.recv_from(&mut buf).await else { continue };
        sessions.retain(|_, (_, last, _)| last.lock().unwrap().elapsed() < IDLE);
        let session = match sessions.get(&client) {
            Some((upstream, last, _)) => {
                *last.lock().unwrap() = std::time::Instant::now();
                upstream.clone()
            }
            None => {
                let Some(ip) = *crate::routes::GUEST_IP.lock().unwrap() else { continue };
                let Ok(upstream) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else { continue };
                if upstream.connect((ip, port)).await.is_err() {
                    continue;
                }
                let upstream = Arc::new(upstream);
                let last = Arc::new(Mutex::new(std::time::Instant::now()));
                // Replies back to this client until the session goes idle.
                let (reply_up, reply_down, reply_last) = (upstream.clone(), socket.clone(), last.clone());
                let replies = tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        match tokio::time::timeout(IDLE, reply_up.recv(&mut buf)).await {
                            Ok(Ok(n)) => {
                                *reply_last.lock().unwrap() = std::time::Instant::now();
                                let _ = reply_down.send_to(&buf[..n], client).await;
                            }
                            _ if reply_last.lock().unwrap().elapsed() < IDLE => continue,
                            _ => break,
                        }
                    }
                });
                sessions.insert(client, (upstream.clone(), last, AbortOnDrop(replies)));
                upstream
            }
        };
        let _ = session.send(&buf[..n]).await;
    }
}

async fn forward(listener: TcpListener, port: u16, direct: bool) {
    let target = format!("tcp 127.0.0.1:{port}");
    loop {
        let Ok((mut client, _)) = listener.accept().await else { continue };
        let target = target.clone();
        tokio::spawn(async move {
            let _ = client.set_nodelay(true);
            let result = async {
                let guest_ip = if direct { *crate::routes::GUEST_IP.lock().unwrap() } else { None };
                if let Some(ip) = guest_ip
                    && let Ok(mut guest) = tokio::net::TcpStream::connect((ip, port)).await
                {
                    guest.set_nodelay(true)?;
                    tokio::io::copy_bidirectional_with_sizes(&mut client, &mut guest, crate::BUF, crate::BUF).await?;
                    return anyhow::Ok(());
                }
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
    docker_get_from(DOCKER_TARGET, path).await
}

async fn docker_get_from(target: &str, path: &str) -> Result<BufReader<UnixStream>> {
    let mut stream = guest_stream(target).await?;
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
