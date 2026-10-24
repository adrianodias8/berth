//! Publish container ports on the Mac. Follows the Docker event stream and
//! keeps one host listener per published TCP port, forwarding each connection
//! to the same port inside the guest (where dockerd's proxy picks it up).
//!
//! Host-network containers (`--net host`) publish nothing: their servers
//! listen in the guest's own network namespace. The agent watches for those
//! (agent/src/listeners.rs), asks for a sync when they change, and lists them
//! with `info listeners`; they are forwarded the same way, on the Mac's
//! loopback only (as OrbStack does): a container server on all interfaces
//! shouldn't appear on the Mac's LAN unless it's published.
//!
//! Syncs run one at a time, coalesced, and failed binds (a port held on the
//! Mac) are retried every few seconds.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Where a forwarded connection goes in the guest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Route {
    /// Try the guest's NAT address first: the server listens on all guest
    /// interfaces. Otherwise only the agent (vsock) can reach it.
    direct: bool,
    /// The address the agent connects to on the vsock path.
    guest: &'static str,
}

/// Published ports: dockerd's proxy listens on 127.0.0.1 in the guest, and
/// also on the NAT address when published on all interfaces.
fn published_route(bind: &str) -> Route {
    Route { direct: bind == "::", guest: "127.0.0.1" }
}

static FORWARDS: Mutex<BTreeMap<Key, (Route, JoinHandle<()>)>> = Mutex::new(BTreeMap::new());
/// One sync at a time; PENDING marks a request not yet covered by one.
static SYNC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PENDING: AtomicBool = AtomicBool::new(false);
/// The last sync couldn't bind everything.
static FAILED: AtomicBool = AtomicBool::new(false);
const RETRY: Duration = Duration::from_secs(5);

/// A listening socket of a host-network container, as the agent reports it.
#[derive(Deserialize, Debug)]
struct Listener {
    proto: String,
    port: u16,
    /// "0.0.0.0", "::", "127.0.0.1" or "::1".
    bind: String,
}

/// Where a host-network listener appears on the Mac: always on loopback.
/// Wildcard binds get both 127.0.0.1 and ::1; loopback ones stay on their
/// family (so a server on `localhost` that binds both gets both). UDP is
/// relayed to the guest's NAT address, so only wildcard binds work.
fn listener_forward(l: &Listener) -> Vec<(Key, Route)> {
    let proto = if l.proto == "udp" { Proto::Udp } else { Proto::Tcp };
    let both = |route: Route| vec![(("127.0.0.1", l.port, proto), route), (("::1", l.port, proto), route)];
    match (l.proto.as_str(), l.bind.as_str()) {
        ("tcp", "0.0.0.0") => both(Route { direct: true, guest: "127.0.0.1" }),
        ("tcp", "::") => both(Route { direct: true, guest: "[::1]" }),
        ("tcp", "127.0.0.1") => vec![(("127.0.0.1", l.port, proto), Route { direct: false, guest: "127.0.0.1" })],
        ("tcp", "::1") => vec![(("::1", l.port, proto), Route { direct: false, guest: "[::1]" })],
        ("udp", "0.0.0.0" | "::") => both(Route { direct: true, guest: "127.0.0.1" }),
        _ => Vec::new(),
    }
}

/// Everything to forward: published ports first; a host-network listener
/// only where no published port already holds that Mac address and port
/// (a published `[::]` holds every address).
fn plan(published: Vec<Key>, listeners: &[Listener]) -> BTreeMap<Key, Route> {
    let mut wanted: BTreeMap<Key, Route> = published.into_iter().map(|key| (key, published_route(key.0))).collect();
    for (key, route) in listeners.iter().flat_map(listener_forward) {
        if !wanted.contains_key(&("::", key.1, key.2)) {
            wanted.entry(key).or_insert(route);
        }
    }
    wanted
}

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
        "::1" => format!("[::1]:{port}{suffix}"),
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
    // Retry ports that couldn't be bound (something on the Mac held them).
    tokio::spawn(async {
        loop {
            tokio::time::sleep(RETRY).await;
            if FAILED.load(Ordering::SeqCst) {
                let _ = sync().await;
            }
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

/// Bring the forwards up to date. Concurrent calls coalesce: each returns
/// once a sync that started after it was called has finished.
async fn sync() -> Result<()> {
    PENDING.store(true, Ordering::SeqCst);
    let _one = SYNC.lock().await;
    if !PENDING.swap(false, Ordering::SeqCst) {
        return Ok(()); // a sync that started after our request just ran
    }
    sync_now().await
}

async fn sync_now() -> Result<()> {
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

    // Host-network containers' servers; absent from an older agent.
    let mut listeners = String::new();
    if let Ok(mut guest) = guest_stream("info listeners").await {
        let _ = guest.read_to_string(&mut listeners).await;
    }
    let listeners: Vec<Listener> = serde_json::from_str(&listeners).unwrap_or_default();

    let mut published: Vec<Key> = containers
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
        published.push(("127.0.0.1", crate::k8s::API_PORT, Proto::Tcp));
    }
    let wanted = plan(published, &listeners);

    // Stop what's no longer wanted, and wait for those tasks to end so their
    // sockets are closed before anything binds the same port again.
    let stopped: Vec<JoinHandle<()>> = {
        let mut forwards = FORWARDS.lock().unwrap();
        let gone: Vec<Key> = forwards.iter().filter(|(key, (route, _))| wanted.get(key) != Some(route)).map(|(k, _)| *k).collect();
        gone.into_iter()
            .filter_map(|key| {
                eprintln!("ports: stopped forwarding {}", display(key));
                forwards.remove(&key).map(|(_, task)| task)
            })
            .collect()
    };
    for task in stopped {
        task.abort();
        let _ = task.await;
    }
    let missing: Vec<(Key, Route)> = {
        let forwards = FORWARDS.lock().unwrap();
        wanted.into_iter().filter(|(key, _)| !forwards.contains_key(key)).collect()
    };
    let mut failed = false;
    for (key, route) in missing {
        // Bind before spawning so a conflict is reported (and retried) now;
        // off the async workers, since the root helper may be asked.
        let bound = tokio::task::spawn_blocking(move || match key.2 {
            Proto::Tcp => bind_tcp(key.0, key.1).map(Bound::Tcp),
            Proto::Udp => bind_udp(key.0, key.1).map(Bound::Udp),
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        let task = bound.and_then(|bound| match bound {
            Bound::Tcp(std) => {
                std.set_nonblocking(true)?;
                Ok(tokio::spawn(forward(TcpListener::from_std(std)?, key.1, route)))
            }
            Bound::Udp(std) => {
                std.set_nonblocking(true)?;
                Ok(tokio::spawn(forward_udp(tokio::net::UdpSocket::from_std(std)?, key.1)))
            }
        });
        match task {
            Ok(task) => {
                eprintln!("ports: forwarding {}", display(key));
                FORWARDS.lock().unwrap().insert(key, (route, task));
            }
            Err(err) => {
                // Logged once per failure streak, not every retry.
                if !FAILED.load(Ordering::SeqCst) {
                    eprintln!("ports: cannot listen on {}: {err} (retrying)", display(key));
                }
                failed = true;
            }
        }
    }
    FAILED.store(failed, Ordering::SeqCst);
    Ok(())
}

enum Bound {
    Tcp(std::net::TcpListener),
    Udp(std::net::UdpSocket),
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
        let Ok((n, client)) = socket.recv_from(&mut buf).await else {
            // Don't spin on a persistent error.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        };
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

/// When a direct connection to the guest last timed out.
static DIRECT_DOWN: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn direct_down() -> bool {
    DIRECT_DOWN.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(10))
}

async fn forward(listener: TcpListener, port: u16, route: Route) {
    let target = format!("tcp {}:{port}", route.guest);
    let direct = route.direct;
    loop {
        let (mut client, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                // Out of descriptors, typically: back off instead of spinning.
                eprintln!("ports: {port}: accept: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let target = target.clone();
        tokio::spawn(async move {
            let _ = client.set_nodelay(true);
            let result = async {
                let guest_ip = if direct && !direct_down() { *crate::routes::GUEST_IP.lock().unwrap() } else { None };
                if let Some(ip) = guest_ip {
                    match tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect((ip, port))).await {
                        Ok(Ok(mut guest)) => {
                            guest.set_nodelay(true)?;
                            tokio::io::copy_bidirectional_with_sizes(&mut client, &mut guest, crate::BUF, crate::BUF).await?;
                            return anyhow::Ok(());
                        }
                        // No answer at all (a stale address after a restore, say):
                        // don't make every connection wait for it.
                        Err(_) => *DIRECT_DOWN.lock().unwrap() = Some(std::time::Instant::now()),
                        Ok(Err(_)) => {}
                    }
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

/// IDs of the running Docker containers.
pub async fn running_containers() -> Result<Vec<String>> {
    let mut body = String::new();
    docker_get("/containers/json").await?.read_to_string(&mut body).await?;
    let list: Vec<serde_json::Value> = serde_json::from_str(&body)?;
    Ok(list.iter().filter_map(|c| c["Id"].as_str().map(str::to_string)).collect())
}

/// Start a Docker container (ignoring "already started").
pub async fn start_container(id: &str) -> Result<()> {
    let mut stream = guest_stream(DOCKER_TARGET).await?;
    stream.write_all(format!("POST /containers/{id}/start HTTP/1.0\r\nHost: docker\r\n\r\n").as_bytes()).await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    let status = reply.split_whitespace().nth(1).unwrap_or("");
    if !matches!(status, "204" | "304") {
        anyhow::bail!("{}", reply.lines().last().unwrap_or(status));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn listener(proto: &str, port: u16, bind: &str) -> Listener {
        Listener { proto: proto.into(), port, bind: bind.into() }
    }

    #[test]
    fn host_listeners_keep_their_scope() {
        let all = Route { direct: true, guest: "127.0.0.1" };
        // Wildcard binds: the Mac's loopback only, both families.
        assert_eq!(
            listener_forward(&listener("tcp", 80, "0.0.0.0")),
            vec![(("127.0.0.1", 80, Proto::Tcp), all), (("::1", 80, Proto::Tcp), all)]
        );
        let v6 = Route { direct: true, guest: "[::1]" };
        assert_eq!(listener_forward(&listener("tcp", 80, "::")), vec![(("127.0.0.1", 80, Proto::Tcp), v6), (("::1", 80, Proto::Tcp), v6)]);
        assert_eq!(
            listener_forward(&listener("tcp", 5173, "::1")),
            vec![(("::1", 5173, Proto::Tcp), Route { direct: false, guest: "[::1]" })]
        );
        assert_eq!(
            listener_forward(&listener("tcp", 3000, "127.0.0.1")),
            vec![(("127.0.0.1", 3000, Proto::Tcp), Route { direct: false, guest: "127.0.0.1" })]
        );
        assert_eq!(
            listener_forward(&listener("udp", 5000, "::")),
            vec![(("127.0.0.1", 5000, Proto::Udp), all), (("::1", 5000, Proto::Udp), all)]
        );
        // Loopback UDP can't be relayed to the NAT address.
        assert_eq!(listener_forward(&listener("udp", 5000, "127.0.0.1")), vec![]);
        assert_eq!(listener_forward(&listener("sctp", 1, "::")), vec![]);
    }

    #[test]
    fn published_ports_win() {
        let published = vec![("127.0.0.1", 80, Proto::Tcp), ("::", 8080, Proto::Tcp)];
        let listeners = [listener("tcp", 8080, "0.0.0.0"), listener("tcp", 80, "0.0.0.0"), listener("tcp", 8080, "::")];
        let plan = plan(published, &listeners);
        assert_eq!(plan.len(), 3, "{plan:?}");
        assert_eq!(plan[&("127.0.0.1", 80, Proto::Tcp)], published_route("127.0.0.1"));
        assert_eq!(plan[&("::", 8080, Proto::Tcp)], published_route("::"));
        // The host-network server on 80 still gets ::1; 8080 is all taken.
        assert_eq!(plan[&("::1", 80, Proto::Tcp)], Route { direct: true, guest: "127.0.0.1" });
    }
}
