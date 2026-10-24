mod cacerts;
mod cli_tools;
mod config;
mod context;
mod daemon;
mod dockerapi;
mod engines;
mod files;
mod fuse;
mod helper;
mod import;
mod k8s;
mod macexec;
mod machines;
mod memory;
mod paths;
mod ports;
mod provision;
mod proxy;
mod routes;
mod snapshots;
mod sshconfig;
mod tls;
mod virtiofs;
mod vm;
mod watch;

use std::io::{BufRead, BufReader, IsTerminal, Write};
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
    /// Stop the VM: shut it down, or with "stop": "suspend" in
    /// ~/.berth/config.json (or --suspend) save it so the next start
    /// resumes it, running containers included
    Stop {
        /// Save the VM's state for the next start to resume
        #[arg(long, conflicts_with = "shutdown")]
        suspend: bool,
        /// Shut the guest down (the default unless the config says suspend)
        #[arg(long)]
        shutdown: bool,
    },
    /// Stop (shutting down, so settings apply) and start again
    Restart,
    /// Open ~/Berth (volumes, machines, containers) in Finder, or one
    /// container's, volume's or machine's files
    Open { name: Option<String> },
    /// Show VM and Docker status (exits 1 when stopped)
    Status,
    /// Versions, settings, paths and what's installed, for bug reports
    Info,
    /// Show berth's logs: daemon (the Mac side), console (the VM's), helper
    Logs {
        #[arg(default_value = "daemon", value_parser = ["daemon", "console", "helper"])]
        which: String,
        /// Keep printing new lines
        #[arg(short, long)]
        follow: bool,
        /// Lines from the end
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Shell completions: berth completions zsh > ~/.zfunc/_berth (bash, zsh, fish)
    Completions { shell: clap_complete::Shell },
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
    /// A root shell inside a container with the VM's tools (bash, ps, ss,
    /// curl, apt...), even in images that have none; its files are at
    /// /container. Nothing is added to the container
    Debug {
        container: String,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Copy images, volumes, networks and containers from OrbStack or Docker
    /// Desktop (which must be running; nothing in it is changed)
    Import {
        /// orbstack or docker-desktop
        from: String,
        /// Another engine's Docker socket instead
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// Only images (combine with --volumes, --containers)
        #[arg(long)]
        images: bool,
        #[arg(long)]
        volumes: bool,
        /// Networks and containers (containers are created stopped)
        #[arg(long)]
        containers: bool,
        /// Show what would be imported
        #[arg(long)]
        dry_run: bool,
        /// Only one Compose project: its containers and the images,
        /// volumes and networks they use
        #[arg(long)]
        project: Option<String>,
    },
    /// Settings in ~/.berth/config.json (docs/configuration.md)
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Open a shell in the VM, or run a command there
    Shell {
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Save the whole VM and go back to it later: containers, volumes,
    /// images, machines, running processes
    Snapshot {
        #[command(subcommand)]
        action: SnapshotCmd,
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
        /// amd64 for an Intel (x86-64) machine, run with Rosetta
        #[arg(long, default_value = "arm64")]
        arch: String,
        /// cloud-init user data to apply on first boot (the image's cloud variant)
        #[arg(long)]
        cloud_init: Option<std::path::PathBuf>,
        /// No home folder, `mac` command or SSH agent from the Mac
        #[arg(long)]
        isolated: bool,
        /// Share only this Mac folder (repeatable; implies --isolated)
        #[arg(long = "mount", value_name = "FOLDER")]
        mounts: Vec<std::path::PathBuf>,
        /// Also keep it off the Mac, the LAN and private networks (DNS and
        /// the internet only; implies --isolated)
        #[arg(long)]
        isolate_network: bool,
    },
    /// Stop and start a machine
    Restart { name: String },
    /// Rename a machine (restarted if it was running)
    Rename { name: String, new_name: String },
    /// A machine's OS, architecture, addresses, disk use and isolation
    Info { name: String },
    /// A machine's system log
    Logs {
        name: String,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: u32,
    },
    /// Copy a file or folder from the Mac into a machine (to your home there)
    Push { name: String, from: std::path::PathBuf, to: Option<String> },
    /// Copy a file or folder from a machine to the Mac (to the current folder)
    Pull { name: String, from: String, to: Option<std::path::PathBuf> },
    /// List machines
    List,
    /// Open a shell in a machine, or run a command there
    Shell {
        name: String,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Start a stopped machine
    Start { name: String },
    /// Stop a machine (it shuts down cleanly)
    Stop { name: String },
    /// Delete a machine and its files (asks first in a terminal)
    Delete {
        name: String,
        /// Don't ask
        #[arg(short, long)]
        yes: bool,
    },
    /// SSH's ProxyCommand for `<machine>.berth` (see ~/.berth/ssh/config)
    #[command(hide = true)]
    Proxy { host: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Every setting, with defaults for those not set
    List,
    Get { key: String },
    Set { key: String, value: String },
    /// Back to the default
    Unset { key: String },
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// Take a snapshot (instant copy-on-write; a running VM keeps running)
    Create {
        name: String,
        /// Save only the disk, not memory and running processes
        #[arg(long)]
        disk_only: bool,
    },
    /// List snapshots
    List,
    /// Go back to a snapshot: everything since is discarded
    Restore {
        name: String,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Delete a snapshot
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
        Cmd::Stop { suspend, shutdown } => {
            let suspend = suspend || (!shutdown && stop_suspends(&paths));
            stop_with(&paths, !suspend)
        }
        Cmd::Status => status(&paths),
        Cmd::Open { name } => {
            let root = files::dir(&paths);
            let target = match name {
                None => root,
                Some(n) => ["containers", "machines"]
                    .iter()
                    .map(|kind| root.join(kind).join(&n))
                    .chain(std::iter::once(root.join("volumes").join(&n).join("_data")))
                    .find(|p| p.exists())
                    .with_context(|| format!("no running container, machine or volume named {n} in {}", root.display()))?,
            };
            if !target.exists() {
                bail!("{} isn't mounted (is berth running? `\"finder\": false`?)", target.display());
            }
            let status = Command::new("open").arg(&target).status()?;
            if !status.success() {
                bail!("open {} failed", target.display());
            }
            Ok(())
        }
        Cmd::Restart => {
            stop(&paths)?;
            start(&paths)
        }
        Cmd::Info => info(&paths),
        Cmd::Logs { which, follow, lines } => {
            let file = match which.as_str() {
                "console" => paths.console_log(),
                "helper" => std::path::PathBuf::from("/var/log/dev.berth.helper.log"),
                _ => paths.daemon_log(),
            };
            let mut tail = Command::new("tail");
            tail.arg("-n").arg(lines.to_string());
            if follow {
                tail.arg("-F");
            }
            Err(tail.arg(file).exec().into())
        }
        Cmd::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "berth", &mut std::io::stdout());
            Ok(())
        }
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
                MachineCmd::Create { name, distro, arch, cloud_init, isolated, mounts, isolate_network } => machines::create(
                    &paths,
                    &name,
                    &distro,
                    &arch,
                    cloud_init.as_deref(),
                    machines::Isolation { isolated: isolated || !mounts.is_empty() || isolate_network, network: isolate_network, mounts },
                ),
                MachineCmd::Restart { name } => machines::simple(&paths, "restart", &name),
                MachineCmd::Rename { name, new_name } => machines::rename(&paths, &name, &new_name),
                MachineCmd::Info { name } => machines::info(&paths, &name),
                MachineCmd::Logs { name, lines } => machines::logs(&paths, &name, lines),
                MachineCmd::Push { name, from, to } => machines::push(&paths, &name, &from, to.as_deref()),
                MachineCmd::Pull { name, from, to } => machines::pull(&paths, &name, &from, to.as_deref()),
                MachineCmd::List => machines::list(&paths),
                MachineCmd::Shell { name, command } => {
                    let remote = machines::shell_command(&name, &command)?;
                    ssh(&paths, &remote, command.is_empty())
                }
                MachineCmd::Start { name } => machines::simple(&paths, "start", &name),
                MachineCmd::Stop { name } => machines::simple(&paths, "stop", &name),
                MachineCmd::Delete { name, yes } => {
                    if !yes && std::io::stdin().is_terminal() {
                        print!("Delete machine {name} and everything in it? [y/N] ");
                        std::io::stdout().flush()?;
                        let mut answer = String::new();
                        std::io::stdin().read_line(&mut answer)?;
                        if !answer.trim().eq_ignore_ascii_case("y") {
                            bail!("not deleted");
                        }
                    }
                    machines::simple(&paths, "delete", &name)
                }
                MachineCmd::Proxy { host } => machine_proxy(&paths, &host),
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
        Cmd::Snapshot { action } => snapshot(&paths, action),
        Cmd::Shell { command } => shell(&paths, &command),
        Cmd::Debug { container, command } => debug(&paths, &container, &command),
        Cmd::Config { action } => match action {
            ConfigCmd::List => config::list(&paths),
            ConfigCmd::Get { key } => config::get(&paths, &key),
            ConfigCmd::Set { key, value } => config::set(&paths, &key, &value),
            ConfigCmd::Unset { key } => config::unset(&paths, &key),
        },
        Cmd::Import { from, socket, images, volumes, containers, dry_run, project } => {
            if current_status(&paths).is_none() {
                bail!("berth is not running: berth start");
            }
            let all = !(images || volumes || containers);
            let source = import::source_socket(&paths, &from, socket)?;
            import::run(
                &paths,
                &source,
                import::Options { images: all || images, volumes: all || volumes, containers: all || containers, dry_run, project },
            )
        }
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

fn snapshot(paths: &Paths, action: SnapshotCmd) -> Result<()> {
    match action {
        SnapshotCmd::Create { name, disk_only } => {
            snapshots::valid_name(&name)?;
            let t = Instant::now();
            let meta = if current_status(paths).is_some() {
                println!("Saving {}…", if disk_only { "the VM's disk" } else { "the VM (memory and disk)" });
                let reply: daemon::SnapshotReply = request_with_timeout(
                    paths,
                    &Request::Snapshot { name: name.clone(), memory: !disk_only },
                    Duration::from_secs(600),
                )?;
                match (reply.meta, reply.error) {
                    (Some(meta), _) => meta,
                    (None, err) => bail!("{}", err.unwrap_or_default()),
                }
            } else {
                let _lock = offline_lock(paths)?;
                snapshots::create_stopped(paths, &name)?
            };
            let what = if meta.with_memory { "memory and disk" } else { "disk" };
            println!("Snapshot {name} saved ({what}) in {:.1}s.", t.elapsed().as_secs_f64());
            if !disk_only && !meta.with_memory {
                println!(
                    "Running processes weren't saved: macOS only saves a VM's state while the Mac is unlocked \
                     (details in ~/.berth/log/daemon.log). Restoring boots this disk and restarts the containers \
                     that were running."
                );
            }
            Ok(())
        }
        SnapshotCmd::List => {
            let all = snapshots::list(paths);
            if all.is_empty() {
                println!("No snapshots. Take one with: berth snapshot create <name>");
            }
            for meta in all {
                let age = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
                    .saturating_sub(meta.created);
                let age = match age {
                    a if a < 120 => format!("{a} s ago"),
                    a if a < 7200 => format!("{} min ago", a / 60),
                    a if a < 172_800 => format!("{} h ago", a / 3600),
                    a => format!("{} days ago", a / 86_400),
                };
                let size = snapshots::size_on_disk(paths, &meta.name) as f64 / (1u64 << 30) as f64;
                let kind = if meta.with_memory { "memory + disk" } else { "disk" };
                println!("{:<24} {:<14} {kind:<14} {size:.1} GiB (shared with the VM until it changes)", meta.name, age);
            }
            Ok(())
        }
        SnapshotCmd::Restore { name, yes } => {
            let meta = snapshots::get(paths, &name)?;
            if !yes {
                print!("Restore {name}? Everything in the VM since then is lost. [y/N] ");
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !answer.trim().eq_ignore_ascii_case("y") {
                    bail!("not restored");
                }
            }
            if current_status(paths).is_some() {
                stop(paths)?;
            }
            {
                let _lock = offline_lock(paths)?;
                snapshots::prepare_restore(paths, &meta.name)?;
            }
            start(paths)?;
            println!("Restored {name}.");
            Ok(())
        }
        SnapshotCmd::Delete { name } => {
            snapshots::delete(paths, &name)?;
            println!("Deleted snapshot {name}.");
            Ok(())
        }
    }
}

/// The daemon's lock, for changing the VM's files while it's stopped: a
/// `berth start` meanwhile would boot a half-copied disk.
fn offline_lock(paths: &Paths) -> Result<std::fs::File> {
    paths.create_dirs()?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths.pid_file())?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("berth is starting or running; try again once it's stopped");
    }
    Ok(file)
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
    let spawn = || -> Result<std::process::Child> {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("daemon").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log.try_clone()?).process_group(0);
        if first_boot {
            cmd.env("BERTH_FIRST_BOOT", "1");
        }
        cmd.spawn().context("spawning daemon")
    };
    let mut child = spawn()?;
    let mut retried = false;

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
            // The watchdog gave up on this boot and asked for a safe one.
            if !retried && vm::safe_boot(paths) {
                retried = true;
                print!("\nThe VM didn't come up; starting it in safe mode");
                std::io::stdout().flush()?;
                child = spawn()?;
                continue;
            }
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
    match sshconfig::install(paths) {
        Ok(true) => println!("Added `Include ~/.berth/ssh/config` to ~/.ssh/config: ssh berth, ssh <machine>.berth"),
        Ok(false) => {}
        Err(err) => eprintln!("Could not write the SSH config: {err:#}"),
    }
    Ok(())
}

/// Connect stdin/stdout to a machine's sshd: SSH's ProxyCommand.
fn machine_proxy(paths: &Paths, host: &str) -> Result<()> {
    let name = host.strip_suffix(".berth").unwrap_or(host);
    let mut sock = std::os::unix::net::UnixStream::connect(paths.machine_ssh_socket())?;
    writeln!(sock, "{name}")?;
    let mut to_machine = sock.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut to_machine);
        let _ = to_machine.shutdown(std::net::Shutdown::Write);
    });
    // Straight to fd 1: std's stdout is line-buffered, and SSH packets
    // would wait in it for a newline byte.
    let mut stdout = std::mem::ManuallyDrop::new(unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(1) });
    std::io::copy(&mut sock, &mut *stdout)?;
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

/// Whether `berth stop` suspends by default ("stop": "suspend" in config.json).
fn stop_suspends(paths: &Paths) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Config {
        stop: Option<String>,
    }
    std::fs::read(paths.config())
        .ok()
        .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
        .and_then(|c| c.stop)
        .is_some_and(|s| s == "suspend")
}

/// Stop with a guest shutdown (restores and other internal stops).
fn stop(paths: &Paths) -> Result<()> {
    stop_with(paths, true)
}

fn stop_with(paths: &Paths, shutdown: bool) -> Result<()> {
    if current_status(paths).is_none() {
        println!("berth is not running");
        return Ok(());
    }
    let _: serde_json::Value = request(paths, &Request::Stop { shutdown })?;
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
    if snapshots::is_suspended(paths) {
        println!(" suspended (the next start resumes it)");
    } else {
        println!(" stopped");
    }
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

/// What a bug report needs, running or not.
fn info(paths: &Paths) -> Result<()> {
    let os = Command::new("sw_vers").arg("-productVersion").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let arch = Command::new("uname").arg("-m").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    println!("berth:    {} (agent {})", env!("CARGO_PKG_VERSION"), &provision::agent_version()[..12.min(provision::agent_version().len())]);
    println!("macOS:    {os} ({arch})");
    println!("binary:   {}", std::env::current_exe()?.display());
    match current_status(paths) {
        Some(s) => println!("VM:       {} ({} CPUs, {} GiB max), Docker {}", s.vm, s.cpus, s.memory >> 30, ready(s.docker)),
        None => println!(
            "VM:       stopped{}",
            if snapshots::is_suspended(paths) { " (suspended: the next start resumes it)" } else { "" }
        ),
    }
    println!(
        "Files:    {} sharing{}",
        virtiofs::configured_share(paths),
        if paths.guest().join("kmod-ready").exists() { ", polling driver built" } else { "" }
    );
    let helper = std::path::Path::new(helper::SOCKET).exists();
    println!("Helper:   {}", if helper { "installed" } else { "not installed (sudo berth helper install)" });
    println!("Snapshots: {}", snapshots::list(paths).len());
    println!("Data:     {}", paths.root.display());
    println!("Settings ({}):", paths.config().display());
    config::list(paths)
}

fn status(paths: &Paths) -> Result<()> {
    match current_status(paths) {
        None => {
            println!("berth is not running");
            // For scripts: `berth status >/dev/null || berth start`.
            std::process::exit(1);
        }
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
            if !s.file_sharing.is_empty() {
                println!("Files:   {}", s.file_sharing);
            }
            if !s.file_sharing_note.is_empty() {
                println!("         note: {}", s.file_sharing_note);
            }
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

/// `berth debug`: guest/berth-debug.sh, installed by the agent.
fn debug(paths: &Paths, container: &str, command: &[String]) -> Result<()> {
    if current_status(paths).is_none() {
        bail!("berth is not running");
    }
    // Names and IDs as Docker and Podman allow them.
    if container.is_empty() || !container.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')) {
        bail!("bad container name: {container}");
    }
    let quote = |w: &str| format!("'{}'", w.replace('\'', "'\\''"));
    let mut remote = vec!["sudo".to_string(), "/usr/local/libexec/berth-debug".into(), container.into()];
    // ssh hands the joined arguments to the VM's shell.
    remote.extend(command.iter().map(|w| quote(w)));
    ssh(paths, &remote, command.is_empty() || std::io::stdin().is_terminal())
}

/// SSH into the VM, optionally running `command`; `tty` for interactive use.
fn ssh(paths: &Paths, command: &[String], tty: bool) -> Result<()> {
    // SSH rides the daemon's vsock bridge, so it needs no guest IP address.
    let user = std::env::var("USER")?;
    let proxy = format!("ProxyCommand=nc -U \"{}\"", paths.ssh_socket().display());
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
