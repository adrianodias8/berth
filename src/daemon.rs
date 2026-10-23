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
}

static STOPPING: AtomicBool = AtomicBool::new(false);

pub fn run(paths: Paths) -> Result<()> {
    paths.create_dirs()?;
    std::fs::write(paths.pid_file(), std::process::id().to_string())?;
    for sock in [paths.control_socket(), paths.docker_socket(), paths.ssh_socket()] {
        let _ = std::fs::remove_file(sock);
    }

    let res = Resources::load(&paths);
    eprintln!("starting VM: {} CPUs, {} MiB", res.cpus, res.memory >> 20);
    let (cpus, memory) = (res.cpus, res.memory);
    vm::create_and_start(&paths, &res, |result| match result {
        Ok(()) => eprintln!("VM started"),
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
    tokio::spawn(ports::run());
    tokio::spawn(serve_host_connections());

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
            })?
        }
        Request::Domains => {
            let mut guest = guest_stream("info domains").await?;
            let mut json = String::new();
            guest.read_to_string(&mut json).await?;
            json
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
                            tokio::io::copy_bidirectional(&mut client, &mut guest).await?;
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
async fn serve_host_connections() {
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
            if let Err(err) = host_connection(fd, tls).await
                && !is_disconnect(&err)
            {
                eprintln!("host connection: {err:#}");
            }
        });
    }
}

async fn host_connection(fd: std::os::fd::OwnedFd, tls: Option<tokio_rustls::TlsAcceptor>) -> Result<()> {
    let std = StdUnixStream::from(fd);
    std.set_nonblocking(true)?;
    let mut guest = UnixStream::from_std(std)?;
    // Byte at a time: whatever follows the header (e.g. a TLS ClientHello)
    // must stay in the stream.
    let mut header = Vec::new();
    loop {
        let byte = guest.read_u8().await?;
        if byte == b'\n' || header.len() > 256 {
            break;
        }
        header.push(byte);
    }
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
            tokio::io::copy_bidirectional(&mut guest, &mut mac).await?;
        }
        ["https", ip, port] => {
            let ip: std::net::Ipv4Addr = ip.parse()?;
            let port: u16 = port.parse()?;
            let tls = tls.ok_or_else(|| anyhow::anyhow!("TLS unavailable"))?;
            // Failed handshakes (e.g. a non-.local name, for which we never
            // issue) are the client's problem, not worth logging.
            let Ok(mut client) = tls.accept(guest).await else { return Ok(()) };
            let upstream = if port == 0 { None } else { guest_stream(&format!("tcp {ip}:{port}")).await.ok() };
            match upstream {
                Some(mut upstream) => {
                    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
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
    for path in [paths.control_socket(), paths.docker_socket(), paths.ssh_socket(), paths.pid_file()] {
        let _ = std::fs::remove_file(path);
    }
    std::process::exit(0);
}
