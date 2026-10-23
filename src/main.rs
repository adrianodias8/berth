mod cli_tools;
mod context;
mod daemon;
mod engines;
mod helper;
mod k8s;
mod machines;
mod memory;
mod paths;
mod ports;
mod provision;
mod routes;
mod tls;
mod vm;
mod watch;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use daemon::{Domain, Request, Status};

/// Copy buffer for proxied connections; tokio's 8 KiB default caps throughput.
pub const BUF: usize = 256 * 1024;
use paths::Paths;

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
    /// Container engines next to Docker: Podman and containerd/nerdctl
    Engine {
        #[command(subcommand)]
        action: EngineCmd,
    },
    /// Run nerdctl in the VM (enable it first: berth engine enable nerdctl)
    #[command(disable_help_flag = true)]
    Nerdctl {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Linux machines: full distros with systemd, your user and home folder
    Machine {
        #[command(subcommand)]
        action: MachineCmd,
    },
    /// Kubernetes (k3s, using berth's Docker): enable, disable or show status
    K8s {
        #[command(subcommand)]
        action: K8sCmd,
    },
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
    /// HTTPS for container domains: trust or remove berth's local CA
    Tls {
        #[command(subcommand)]
        action: TlsCmd,
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
enum EngineCmd {
    /// Install and start an engine: podman or nerdctl
    Enable { name: String },
    /// Stop an engine (its containers and images are kept)
    Disable { name: String },
    /// Show which engines are enabled
    Status,
}

#[derive(Subcommand)]
enum MachineCmd {
    /// Create and start a machine
    Create {
        name: String,
        /// distro[:release] from images.linuxcontainers.org, e.g. debian:trixie, fedora:44, alpine
        #[arg(long, default_value = "")]
        distro: String,
    },
    /// List machines
    List,
    /// Open a shell in a machine, or run a command there
    Shell {
        name: String,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    Start { name: String },
    Stop { name: String },
    /// Delete a machine and its files
    Delete { name: String },
}

#[derive(Subcommand)]
enum K8sCmd {
    /// Install and start a single-node cluster; adds the `berth` kubectl context
    Enable,
    /// Stop the cluster and remove its pods
    Disable,
    /// Show whether Kubernetes is running
    Status,
}

#[derive(Subcommand)]
enum TlsCmd {
    /// Trust the berth CA in your login keychain (asks for your password)
    Trust,
    /// Remove the berth CA from your keychain
    Untrust,
    /// Show whether the CA is trusted
    Status,
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
    // Die quietly on a closed pipe (`berth status | head`) instead of panicking.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
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
        Cmd::Engine { action } => {
            if current_status(&paths).is_none() {
                bail!("berth is not running");
            }
            match action {
                EngineCmd::Enable { name } => engines::enable(&paths, &name),
                EngineCmd::Disable { name } => engines::disable(&paths, &name),
                EngineCmd::Status => engines::status(&paths),
            }
        }
        Cmd::Nerdctl { args } => {
            if current_status(&paths).is_none() {
                bail!("berth is not running");
            }
            let tty = unsafe { libc::isatty(0) } == 1 && unsafe { libc::isatty(1) } == 1;
            ssh(&paths, &engines::nerdctl_command(&args), tty)
        }
        Cmd::Machine { action } => {
            if current_status(&paths).is_none() {
                bail!("berth is not running");
            }
            match action {
                MachineCmd::Create { name, distro } => machines::create(&paths, &name, &distro),
                MachineCmd::List => machines::list(&paths),
                MachineCmd::Shell { name, command } => {
                    let remote = machines::shell_command(&name, &command)?;
                    ssh(&paths, &remote, command.is_empty())
                }
                MachineCmd::Start { name } => machines::simple(&paths, "start", &name),
                MachineCmd::Stop { name } => machines::simple(&paths, "stop", &name),
                MachineCmd::Delete { name } => machines::simple(&paths, "delete", &name),
            }
        }
        Cmd::K8s { action } => {
            if current_status(&paths).is_none() {
                bail!("berth is not running");
            }
            match action {
                K8sCmd::Enable => k8s::enable(&paths),
                K8sCmd::Disable => k8s::disable(&paths),
                K8sCmd::Status => {
                    println!("Kubernetes: {}", k8s::status(&paths)?);
                    Ok(())
                }
            }
        }
        Cmd::Shell { command } => shell(&paths, &command),
        Cmd::Delete { yes } => delete(&paths, yes),
        Cmd::Tls { action: TlsCmd::Trust } => tls::trust(&paths),
        Cmd::Tls { action: TlsCmd::Untrust } => tls::untrust(&paths),
        Cmd::Tls { action: TlsCmd::Status } => {
            println!("{}", https_state(&paths));
            Ok(())
        }
        Cmd::Daemon => daemon::run(paths),
        Cmd::Helper { .. } => unreachable!(),
    }
}

fn request<T: serde::de::DeserializeOwned>(paths: &Paths, req: &Request) -> Result<T> {
    request_with_timeout(paths, req, Duration::from_secs(10))
}

pub fn request_with_timeout<T: serde::de::DeserializeOwned>(paths: &Paths, req: &Request, timeout: Duration) -> Result<T> {
    let mut stream = UnixStream::connect(paths.control_socket())?;
    stream.set_read_timeout(Some(timeout))?;
    writeln!(stream, "{}", serde_json::to_string(req)?)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn current_status(paths: &Paths) -> Option<Status> {
    request(paths, &Request::Status).ok()
}

fn start(paths: &Paths) -> Result<()> {
    // Cheap once downloaded; also repairs plugin links if another app went away.
    if let Err(err) = cli_tools::ensure(paths) {
        // Not fatal: an existing docker CLI (or a later start) covers it.
        eprintln!("Could not install the Docker CLIs: {err:#}");
    }
    if let Some(status) = current_status(paths) {
        println!("berth is already running (VM {}, docker {})", status.vm, ready(status.docker));
        use_docker_context(paths);
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
    let mut ticks = 0u32;
    loop {
        // Poll often: this delay adds straight onto the start time.
        std::thread::sleep(Duration::from_millis(200));
        ticks += 1;
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
        if ticks.is_multiple_of(5) {
            print!(".");
            std::io::stdout().flush()?;
        }
    }
    println!(" ready in {:.0?}", started.elapsed());
    use_docker_context(paths);
    Ok(())
}

/// Make `berth` the docker CLI's current context, as OrbStack does on start.
fn use_docker_context(paths: &Paths) {
    match context::use_berth(paths) {
        Ok(()) => println!("Docker context '{}' is now current ({})", context::NAME, paths.docker_socket().display()),
        Err(err) => eprintln!(
            "Could not switch the docker context: {err:#}\nUse: docker context use {}",
            context::NAME
        ),
    }
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

fn https_state(paths: &Paths) -> String {
    if tls::is_trusted(paths) {
        "on (https://<container>.berth.local)".into()
    } else {
        "certificate not trusted yet (run: berth tls trust)".into()
    }
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
            if s.memory_allowed > 0 {
                println!(
                    "Memory:  {:.1} GiB in use, VM holding up to {:.1} GiB (rest returned to macOS)",
                    s.memory_used as f64 / (1u64 << 30) as f64,
                    s.memory_allowed as f64 / (1u64 << 30) as f64
                );
            }
            println!("Socket:  {}", paths.docker_socket().display());
            if !s.ports.is_empty() {
                println!("Ports:   {}", s.ports.join(", "));
            }
            println!("IPs:     {}", s.container_ips);
            println!("HTTPS:   {}", https_state(paths));
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
    ssh(paths, command, command.is_empty())
}

/// SSH into the VM, optionally running `command`; `tty` for interactive use.
fn ssh(paths: &Paths, command: &[String], tty: bool) -> Result<()> {
    // SSH rides the daemon's vsock bridge, so it needs no guest IP address.
    let user = std::env::var("USER")?;
    let proxy = format!("ProxyCommand=nc -U {}", paths.ssh_socket().display());
    let mut ssh = Command::new("ssh");
    ssh.arg("-i")
        .arg(paths.ssh_key())
        .args(["-o", &proxy, "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null"])
        .args(["-o", "LogLevel=ERROR"])
        .arg(format!("{user}@berth"));
    if tty {
        ssh.arg("-t");
    }
    ssh.args(command);
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
    // The exported kernel belongs to the deleted VM.
    std::fs::remove_dir_all(paths.guest().join("boot")).ok();
    println!("Deleted {}", paths.data().display());
    Ok(())
}
