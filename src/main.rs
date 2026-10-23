mod daemon;
mod helper;
mod paths;
mod ports;
mod provision;
mod routes;
mod vm;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use daemon::{Domain, Request, Status};
use paths::Paths;

const DOCKER_CONTEXT: &str = "berth";

#[derive(Parser)]
#[command(name = "berth", version, about = "Run Docker containers in a lightweight Linux VM on macOS")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the VM and Docker engine
    Start,
    /// Shut down the VM
    Stop,
    /// Show VM and Docker status
    Status,
    /// List container domain names and their addresses
    Domains,
    /// Open a shell in the VM, or run a command there
    Shell {
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Delete the VM disk and all containers and images in it
    Delete {
        #[arg(long)]
        yes: bool,
    },
    /// Manage the privileged route helper that makes container IPs reachable
    Helper {
        #[command(subcommand)]
        action: HelperCmd,
    },
    /// Run the VM in the foreground (used internally by `start`)
    #[command(hide = true)]
    Daemon,
}

#[derive(Subcommand)]
enum HelperCmd {
    /// Install the helper (run with sudo)
    Install,
    /// Remove the helper and its routes (run with sudo)
    Uninstall,
    /// Run the helper (used by launchd)
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        uid: u32,
    },
}

fn main() -> Result<()> {
    let command = Cli::parse().command;
    // The route helper runs under launchd as root with no HOME and needs no
    // berth paths, so handle it before touching them.
    if let Cmd::Helper { action } = command {
        return match action {
            HelperCmd::Install => helper::install(),
            HelperCmd::Uninstall => helper::uninstall(),
            HelperCmd::Serve { uid } => helper::serve(uid),
        };
    }
    let paths = Paths::new();
    match command {
        Cmd::Start => start(&paths),
        Cmd::Stop => stop(&paths),
        Cmd::Status => status(&paths),
        Cmd::Domains => domains(&paths),
        Cmd::Shell { command } => shell(&paths, &command),
        Cmd::Delete { yes } => delete(&paths, yes),
        Cmd::Daemon => daemon::run(paths),
        Cmd::Helper { .. } => unreachable!(),
    }
}

fn request<T: serde::de::DeserializeOwned>(paths: &Paths, req: &Request) -> Result<T> {
    let mut stream = UnixStream::connect(paths.control_socket())?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    writeln!(stream, "{}", serde_json::to_string(req)?)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn current_status(paths: &Paths) -> Option<Status> {
    request(paths, &Request::Status).ok()
}

fn start(paths: &Paths) -> Result<()> {
    if let Some(status) = current_status(paths) {
        println!("berth is already running (VM {}, docker {})", status.vm, ready(status.docker));
        return Ok(());
    }
    let first_boot = !paths.disk().exists();
    provision::ensure(paths)?;

    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths.daemon_log())?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("spawning daemon")?;

    if first_boot {
        println!("First boot: installing Docker in the VM. This takes a few minutes.");
    }
    print!("Starting berth");
    std::io::stdout().flush()?;
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(code) = child.try_wait()? {
            println!();
            bail!(
                "daemon exited ({}). See {}",
                code.code().map_or_else(|| format!("signal {:?}", code.signal()), |c| c.to_string()),
                paths.daemon_log().display()
            );
        }
        if current_status(paths).is_some_and(|s| s.docker) {
            break;
        }
        if started.elapsed() > Duration::from_secs(15 * 60) {
            println!();
            bail!(
                "Docker did not come up within 15 minutes. See {} and {}",
                paths.daemon_log().display(),
                paths.console_log().display()
            );
        }
        print!(".");
        std::io::stdout().flush()?;
    }
    println!(" ready in {:.0?}", started.elapsed());
    ensure_docker_context(paths);
    Ok(())
}

fn ensure_docker_context(paths: &Paths) {
    let exists = Command::new("docker")
        .args(["context", "inspect", DOCKER_CONTEXT])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !exists {
        let host = format!("host=unix://{}", paths.docker_socket().display());
        let created = Command::new("docker")
            .args(["context", "create", DOCKER_CONTEXT, "--description", "berth", "--docker", &host])
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !created {
            eprintln!("Could not create docker context '{DOCKER_CONTEXT}'.");
        }
    }
    println!("Docker socket: {}", paths.docker_socket().display());
    println!("Use it with:   docker context use {DOCKER_CONTEXT}   (or: docker --context {DOCKER_CONTEXT} ...)");
}

fn stop(paths: &Paths) -> Result<()> {
    if current_status(paths).is_none() {
        println!("berth is not running");
        return Ok(());
    }
    let _: serde_json::Value = request(paths, &Request::Stop)?;
    print!("Stopping");
    std::io::stdout().flush()?;
    let deadline = Instant::now() + Duration::from_secs(40);
    while paths.control_socket().exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        print!(".");
        std::io::stdout().flush()?;
    }
    if paths.control_socket().exists() {
        println!();
        bail!("daemon still running; see {}", paths.daemon_log().display());
    }
    println!(" stopped");
    Ok(())
}

fn ready(ok: bool) -> &'static str {
    if ok { "ready" } else { "not ready" }
}

fn status(paths: &Paths) -> Result<()> {
    match current_status(paths) {
        None => println!("berth is not running"),
        Some(s) => {
            println!("VM:      {} (pid {}, {} CPUs, {} GiB max)", s.vm, s.pid, s.cpus, s.memory >> 30);
            println!("Docker:  {}", ready(s.docker));
            println!("Socket:  {}", paths.docker_socket().display());
            if !s.ports.is_empty() {
                println!("Ports:   {}", s.ports.join(", "));
            }
            println!("IPs:     {}", s.container_ips);
        }
    }
    Ok(())
}

fn domains(paths: &Paths) -> Result<()> {
    if current_status(paths).is_none() {
        bail!("berth is not running");
    }
    let domains: Vec<Domain> = request(paths, &Request::Domains)?;
    if domains.is_empty() {
        println!("No running containers with domains.");
        return Ok(());
    }
    let width = domains.iter().map(|d| d.domain.len() + 2).max().unwrap_or(0);
    for d in &domains {
        let name = if d.subdomains_only { format!("*.{}", d.domain) } else { d.domain.clone() };
        println!("{name:width$}  {:15}  {}", d.ip, d.container);
    }
    Ok(())
}

fn shell(paths: &Paths, command: &[String]) -> Result<()> {
    if current_status(paths).is_none() {
        bail!("berth is not running");
    }
    // SSH rides the daemon's vsock bridge, so it needs no guest IP address.
    let user = std::env::var("USER")?;
    let proxy = format!("ProxyCommand=nc -U {}", paths.ssh_socket().display());
    let mut ssh = Command::new("ssh");
    ssh.arg("-i")
        .arg(paths.ssh_key())
        .args(["-o", &proxy, "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null"])
        .args(["-o", "LogLevel=ERROR"])
        .arg(format!("{user}@berth"));
    if command.is_empty() {
        ssh.arg("-t");
    } else {
        ssh.args(command);
    }
    Err(ssh.exec().into())
}

fn delete(paths: &Paths, yes: bool) -> Result<()> {
    if !yes {
        bail!("this deletes all containers, images and volumes in berth; re-run with --yes");
    }
    if current_status(paths).is_some() {
        stop(paths)?;
    }
    std::fs::remove_dir_all(paths.data()).ok();
    println!("Deleted {}", paths.data().display());
    Ok(())
}
