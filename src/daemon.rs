//! The background process that owns the VM. The main thread drains the main
//! dispatch queue (where Virtualization.framework lives); a tokio runtime on
//! other threads serves the control socket and the Docker socket.

use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::paths::Paths;
use crate::ports;
use crate::vm::{self, AGENT_VSOCK_PORT, Resources};

/// Agent targets (see agent/src/main.rs).
pub const DOCKER_TARGET: &str = "unix /run/docker.sock";
pub const PODMAN_TARGET: &str = "unix /run/podman/podman.sock";
const SSH_TARGET: &str = "tcp 127.0.0.1:22";

unsafe extern "C" {
    fn dispatch_main() -> !;
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Stop,
    Domains,
    K8s { action: String },
    Machine { action: String, args: Vec<String> },
    Engine { args: String },
    Snapshot { name: String, memory: bool },
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SnapshotReply {
    pub meta: Option<crate::snapshots::Meta>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EngineReply {
    pub output: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct MachineReply {
    pub output: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct K8sReply {
    pub output: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Domain {
    pub domain: String,
    pub wildcard: bool,
    pub subdomains_only: bool,
    pub ip: String,
    pub container: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Status {
    pub pid: u32,
    pub vm: String,
    pub docker: bool,
    pub cpus: usize,
    pub memory: u64,
    #[serde(default)]
    pub ports: Vec<String>,
    /// Direct container IP routing, human-readable.
    #[serde(default)]
    pub container_ips: String,
    /// Memory the guest is using (excluding reclaimable cache).
    #[serde(default)]
    pub memory_used: u64,
    /// Memory the VM may currently hold (balloon target); the rest is returned
    /// to macOS.
    #[serde(default)]
    pub memory_allowed: u64,
    /// How the home folder is shared (virtiofs.rs).
    #[serde(default)]
    pub file_sharing: String,
}

static STOPPING: AtomicBool = AtomicBool::new(false);
/// The VM resumed a snapshot's saved state: its clock is where it was saved.
static RESTORED: AtomicBool = AtomicBool::new(false);

/// Keep the guest's clock on the Mac's: the VM's clock stops while the Mac
/// sleeps, and NTP in the guest takes a long time to notice.
async fn keep_clock() {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        sync_clock().await;
    }
}

async fn sync_clock() {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let Ok(mut guest) = guest_stream(&format!("clock {:.6}", now.as_secs_f64())).await else { return };
    let mut reply = String::new();
    let _ = guest.read_to_string(&mut reply).await;
    if let Ok(drift) = reply.trim().parse::<f64>()
        && drift.abs() > 1.0
    {
        eprintln!("clock: guest was {drift:.1}s off, corrected");
    }
}

/// If the guest agent doesn't answer soon after boot (a broken mount
/// setup leaves it without the home folder it runs from), leave word for a
/// safe boot (vm.rs) and give up; `berth start` retries once that way.
async fn watchdog() {
    let paths = Paths::new();
    let safe = paths.guest().join("boot/safe-boot");
    if std::env::var_os("BERTH_FIRST_BOOT").is_some() {
        return; // provisioning starts the agent only after installing Docker
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if crate::memory::GUEST_USED.load(std::sync::atomic::Ordering::Relaxed) > 0 || guest_stream("info memory").await.is_ok() {
            if crate::vm::safe_boot(&paths) {
                eprintln!("safe boot: up; the agent repaired the guest's mounts");
                let _ = std::fs::remove_file(&safe);
            }
            return;
        }
    }
    if crate::vm::safe_boot(&paths) {
        eprintln!("safe boot: the guest didn't come up either; see {}", paths.console_log().display());
        let _ = std::fs::remove_file(&safe);
        return;
    }
    eprintln!("watchdog: the guest agent didn't come up within 90 s; next start boots in safe mode");
    let _ = std::fs::write(&safe, "");
    let _ = vm::force_stop().await;
    std::process::exit(3);
}

/// The VM booted (rather than resuming a saved state).
static BOOTED: AtomicBool = AtomicBool::new(false);
/// Containers to start once a restored snapshot's disk has booted.
static RESTART_AFTER_BOOT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// After a restore: set the clock of a resumed VM, or start the snapshot's
/// containers on one that booted its disk.
async fn after_restore() {
    for _ in 0..150 {
        if BOOTED.load(std::sync::atomic::Ordering::SeqCst) {
            let ids = std::mem::take(&mut *RESTART_AFTER_BOOT.lock().unwrap());
            if !ids.is_empty() {
                eprintln!("snapshot: starting the {} containers that were running", ids.len());
                crate::snapshots::start_containers(ids).await;
            }
            return;
        }
        if RESTORED.load(std::sync::atomic::Ordering::SeqCst) {
            sync_clock().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// launchd starts processes with 256 descriptors; every forwarded
/// connection and shared file needs one.
fn raise_fd_limit() {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
        // macOS rejects more than kern.maxfilesperproc, even when unlimited.
        let mut per_proc: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let name = c"kern.maxfilesperproc";
        let max = if unsafe { libc::sysctlbyname(name.as_ptr(), (&mut per_proc as *mut libc::c_int).cast(), &mut size, std::ptr::null_mut(), 0) } == 0
            && per_proc > 0
        {
            per_proc as libc::rlim_t
        } else {
            10_240
        };
        lim.rlim_cur = lim.rlim_max.min(max).min(65_536);
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
    }
}

pub fn run(paths: Paths) -> Result<()> {
    paths.create_dirs()?;
    raise_fd_limit();
    // One daemon per data directory: a second would delete the first's
    // sockets and boot the same disk. The lock lives as long as the process.
    let pid_file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths.pid_file())?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&pid_file), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!("berth is already running");
    }
    pid_file.set_len(0)?;
    std::io::Write::write_all(&mut &pid_file, std::process::id().to_string().as_bytes())?;
    std::mem::forget(pid_file);
    for sock in [paths.control_socket(), paths.docker_socket(), paths.ssh_socket(), paths.podman_socket()] {
        let _ = std::fs::remove_file(sock);
    }

    let restore = crate::snapshots::take_restore(&paths);
    let res = match &restore {
        Some(r) => Resources { cpus: r.resources.cpus, memory: r.resources.memory },
        None => Resources::load(&paths),
    };
    let state = restore.as_ref().and_then(|r| r.state.clone());
    *RESTART_AFTER_BOOT.lock().unwrap() = restore.map(|r| r.start).unwrap_or_default();
    eprintln!(
        "{} VM: {} CPUs, {} MiB",
        if state.is_some() { "restoring" } else { "starting" },
        res.cpus,
        res.memory >> 20
    );
    let (cpus, memory) = (res.cpus, res.memory);
    vm::create_and_start(&paths, &res, state.as_deref(), |result| match result {
        Ok(true) => {
            eprintln!("VM restored from snapshot");
            RESTORED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(false) => {
            eprintln!("VM started");
            BOOTED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Err(err) => {
            eprintln!("VM failed to start: {err:#}");
            std::process::exit(1);
        }
    })?;

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            if let Err(err) = serve(paths, cpus, memory).await {
                eprintln!("daemon error: {err:#}");
                std::process::exit(1);
            }
        });
    });

    unsafe { dispatch_main() }
}

async fn serve(paths: Paths, cpus: usize, memory: u64) -> Result<()> {
    let control = UnixListener::bind(paths.control_socket())?;
    bridge(UnixListener::bind(paths.docker_socket())?, DOCKER_TARGET, "docker");
    bridge(UnixListener::bind(paths.ssh_socket())?, SSH_TARGET, "ssh");
    bridge(UnixListener::bind(paths.podman_socket())?, PODMAN_TARGET, "podman");
    crate::watch::start();
    tokio::spawn(ports::run());
    tokio::spawn(crate::memory::run(memory));
    tokio::spawn(crate::routes::run_periodic());
    tokio::spawn(serve_host_connections(memory));
    tokio::spawn(after_restore());
    tokio::spawn(watchdog());
    tokio::spawn(keep_clock());

    let cleanup_paths = Paths::new();
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        shutdown().await;
    });

    // Exit once the guest powers off, however that happened.
    tokio::spawn(async move {
        let mut seen_running = false;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let Ok(state) = vm::state().await else { continue };
            match vm::state_name(state) {
                "running" => seen_running = true,
                "stopped" | "error" if seen_running || STOPPING.load(Ordering::SeqCst) => {
                    eprintln!("VM is {}, exiting", vm::state_name(state));
                    exit(&cleanup_paths);
                }
                _ => {}
            }
        }
    });

    loop {
        let (stream, _) = control.accept().await?;
        tokio::spawn(async move {
            if let Err(err) = handle_control(stream, cpus, memory).await {
                eprintln!("control: {err:#}");
            }
        });
    }
}

async fn handle_control(stream: UnixStream, cpus: usize, memory: u64) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let reply = match serde_json::from_str::<Request>(&line)? {
        Request::Status => {
            let state = vm::state().await?;
            let running = vm::state_name(state) == "running";
            serde_json::to_string(&Status {
                pid: std::process::id(),
                vm: vm::state_name(state).to_string(),
                docker: running && docker_ready().await,
                cpus,
                memory,
                ports: ports::forwarded(),
                container_ips: crate::routes::state(),
                memory_used: crate::memory::GUEST_USED.load(std::sync::atomic::Ordering::Relaxed),
                memory_allowed: crate::memory::TARGET.load(std::sync::atomic::Ordering::Relaxed),
                file_sharing: crate::virtiofs::CURRENT.get().map_or("", |s| s.describe()).to_string(),
            })?
        }
        Request::K8s { action } => {
            let mut guest = guest_stream(&format!("k8s {action}")).await?;
            let mut output = String::new();
            guest.read_to_string(&mut output).await?;
            serde_json::to_string(&K8sReply { output })?
        }
        Request::Machine { action, args } => {
            let mut guest = guest_stream(&format!("machine {action}")).await?;
            guest.write_all(serde_json::to_string(&args)?.as_bytes()).await?;
            guest.shutdown().await?;
            let mut output = String::new();
            guest.read_to_string(&mut output).await?;
            serde_json::to_string(&MachineReply { output })?
        }
        Request::Engine { args } => {
            let mut guest = guest_stream(&format!("engine {args}")).await?;
            let mut output = String::new();
            guest.read_to_string(&mut output).await?;
            serde_json::to_string(&EngineReply { output })?
        }
        Request::Domains => {
            let mut guest = guest_stream("info domains").await?;
            let mut json = String::new();
            guest.read_to_string(&mut json).await?;
            json
        }
        Request::Snapshot { name, memory: with_memory } => {
            let res = Resources { cpus, memory };
            let reply = match crate::snapshots::create_running(&Paths::new(), &name, with_memory, &res).await {
                Ok(meta) => SnapshotReply { meta: Some(meta), error: None },
                Err(err) => SnapshotReply { meta: None, error: Some(format!("{err:#}")) },
            };
            serde_json::to_string(&reply)?
        }
        Request::Stop => {
            tokio::spawn(shutdown());
            "{\"ok\":true}".to_string()
        }
    };
    write.write_all(reply.as_bytes()).await?;
    write.write_all(b"\n").await?;
    Ok(())
}

/// Forward every connection on a host unix socket to a guest agent target.
fn bridge(listener: UnixListener, target: &'static str, name: &'static str) {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut client, _)) => {
                    tokio::spawn(async move {
                        let result = async {
                            let mut guest = guest_stream(target).await?;
                            tokio::io::copy_bidirectional_with_sizes(&mut client, &mut guest, crate::BUF, crate::BUF).await?;
                            anyhow::Ok(())
                        };
                        if let Err(err) = result.await
                            && !is_disconnect(&err)
                        {
                            eprintln!("{name} bridge: {err:#}");
                        }
                    });
                }
                Err(err) => eprintln!("{name} accept: {err}"),
            }
        }
    });
}

/// Guest-to-Mac connections over vsock. The agent sends one header line:
///
///   tcp <port>                  host.docker.internal: connect to that port on
///                               the Mac's loopback, nothing else
///   https <container-ip> <port> HTTPS for a container domain: terminate TLS
///                               here and proxy plain HTTP to the container
///   memory-pressure             the guest is stalling on memory: deflate the
///                               balloon now (memory.rs)
///   sync                        containers changed outside Docker's event
///                               stream (nerdctl), or host-network containers
///                               opened or closed ports: re-sync port forwarding
async fn serve_host_connections(max_memory: u64) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    if vm::INCOMING.set(tx).is_err() {
        return;
    }
    let tls = match crate::tls::Authority::load_or_create(&Paths::new()).and_then(|ca| Arc::new(ca).acceptor()) {
        Ok(acceptor) => Some(acceptor),
        Err(err) => {
            eprintln!("tls: HTTPS for container domains disabled: {err:#}");
            None
        }
    };
    while let Some(fd) = rx.recv().await {
        let tls = tls.clone();
        tokio::spawn(async move {
            if let Err(err) = host_connection(fd, tls, max_memory).await
                && !is_disconnect(&err)
            {
                eprintln!("host connection: {err:#}");
            }
        });
    }
}

async fn host_connection(fd: std::os::fd::OwnedFd, tls: Option<tokio_rustls::TlsAcceptor>, max_memory: u64) -> Result<()> {
    let std = StdUnixStream::from(fd);
    std.set_nonblocking(true)?;
    let mut guest = UnixStream::from_std(std)?;
    // Byte at a time: whatever follows the header (e.g. a TLS ClientHello)
    // must stay in the stream.
    // Anything in the VM can connect here: don't let idle connections pile up.
    let mut header = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let byte = guest.read_u8().await?;
            if byte == b'\n' || header.len() > 256 {
                break;
            }
            header.push(byte);
        }
        anyhow::Ok(())
    })
    .await??;
    let header = String::from_utf8_lossy(&header).into_owned();
    let parts: Vec<&str> = header.split_whitespace().collect();
    match parts.as_slice() {
        ["tcp", port] => {
            let port: u16 = port.parse()?;
            // Many dev servers bind only one of the loopback addresses.
            let mut mac = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => s,
                Err(_) => tokio::net::TcpStream::connect(("::1", port)).await?,
            };
            mac.set_nodelay(true)?;
            tokio::io::copy_bidirectional_with_sizes(&mut guest, &mut mac, crate::BUF, crate::BUF).await?;
        }
        ["https", ip, port] => {
            let ip: std::net::Ipv4Addr = ip.parse()?;
            let port: u16 = port.parse()?;
            let tls = tls.ok_or_else(|| anyhow::anyhow!("TLS unavailable"))?;
            // Failed handshakes (e.g. a non-.local name, for which we never
            // issue) are the client's problem, not worth logging.
            let Ok(Ok(mut client)) = tokio::time::timeout(Duration::from_secs(10), tls.accept(guest)).await else {
                return Ok(());
            };
            let upstream = if port == 0 { None } else { guest_stream(&format!("tcp {ip}:{port}")).await.ok() };
            match upstream {
                Some(mut upstream) => {
                    tokio::io::copy_bidirectional_with_sizes(&mut client, &mut upstream, crate::BUF, crate::BUF).await?;
                }
                None => {
                    let body = "berth: no web server found in this container\n";
                    let response = format!(
                        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    client.write_all(response.as_bytes()).await?;
                    client.shutdown().await?;
                }
            }
        }
        ["memory-pressure"] => crate::memory::on_pressure(max_memory).await,
        ["sync"] => ports::trigger(),
        _ => anyhow::bail!("bad header {header:?}"),
    }
    Ok(())
}

/// A peer hanging up or refusing; routine for proxied connections.
pub fn is_disconnect(err: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|e| matches!(e.kind(), BrokenPipe | ConnectionReset | ConnectionAborted | UnexpectedEof))
}

/// Open a stream to `target` inside the guest, via the agent.
pub async fn guest_stream(target: &str) -> Result<UnixStream> {
    let fd = vm::connect_vsock(AGENT_VSOCK_PORT).await?;
    let std = StdUnixStream::from(fd);
    std.set_nonblocking(true)?;
    let mut stream = UnixStream::from_std(std)?;
    stream.write_all(format!("{target}\n").as_bytes()).await?;
    Ok(stream)
}

async fn docker_ready() -> bool {
    let probe = async {
        let mut guest = guest_stream(DOCKER_TARGET).await?;
        guest.write_all(b"GET /_ping HTTP/1.0\r\n\r\n").await?;
        let mut buf = Vec::new();
        guest.read_to_end(&mut buf).await?;
        anyhow::Ok(buf.starts_with(b"HTTP/1.0 200") || buf.starts_with(b"HTTP/1.1 200"))
    };
    matches!(tokio::time::timeout(Duration::from_secs(2), probe).await, Ok(Ok(true)))
}

/// Graceful guest shutdown with a hard-stop fallback.
async fn shutdown() {
    if STOPPING.swap(true, Ordering::SeqCst) {
        return;
    }
    eprintln!("stopping VM");
    if let Err(err) = vm::request_stop().await {
        eprintln!("stop request failed: {err:#}");
    }
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if matches!(vm::state().await.map(vm::state_name), Ok("stopped" | "error")) {
            exit(&Paths::new());
        }
    }
    eprintln!("guest did not shut down in 20s, forcing");
    let _ = vm::force_stop().await;
    exit(&Paths::new());
}

fn exit(paths: &Paths) -> ! {
    crate::routes::clear();
    for path in [paths.control_socket(), paths.docker_socket(), paths.ssh_socket(), paths.podman_socket(), paths.pid_file()] {
        let _ = std::fs::remove_file(path);
    }
    std::process::exit(0);
}
