//! `berth mac`: macOS VMs for testing Mac apps in a clean, disposable guest
//! (docs/plans/gui-vms-agent-testing.md, phase 1). Each VM is a directory
//! under `~/.berth/mac/` (store.rs) run by its own `berth mac run` process
//! (vz.rs), outside the berth daemon and the Linux VM, as
//! docs/gui-protocol.md ("Transport") describes.
//!
//! The lifecycle: `create` installs macOS from an IPSW and boots it once
//! with an account provisioned (macOS 27), so Setup Assistant never shows;
//! `set-base` saves that clean state; `clone` makes copy-on-write copies of
//! it, each with its own machine identifier and MAC address; `reset` puts a
//! clone (or the base itself) back to the clean state in about a second.
//! Apple's license allows two macOS VMs running at once, which `start`
//! enforces.
//!
//! Experimental: only with `"mac": true` in ~/.berth/config.json.

mod dhcp;
mod store;
mod vz;

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::json;

use crate::paths::Paths;
use store::{Display, Meta, Vm};

#[derive(Subcommand)]
pub enum MacCmd {
    /// Install macOS into a new VM, then boot it once to set up its account
    Create {
        name: String,
        /// Install from this IPSW file
        #[arg(long, conflicts_with = "latest")]
        ipsw: Option<PathBuf>,
        /// Download the newest macOS this Mac supports (the default; ~15 GB, kept in ~/.berth/mac/cache)
        #[arg(long)]
        latest: bool,
        /// CPUs (default 4, at least what the macOS version needs)
        #[arg(long)]
        cpus: Option<usize>,
        /// Memory in MiB (default 8192)
        #[arg(long)]
        memory_mib: Option<u64>,
        /// Disk size in GiB; space is only used as the guest writes
        #[arg(long, default_value_t = 80)]
        disk_gib: u64,
        /// The account to create
        #[arg(long, default_value = "berth")]
        user: String,
        #[arg(long, default_value = "berth")]
        password: String,
        /// The guest's display, in points: WIDTHxHEIGHT@1x or @2x (Retina)
        #[arg(long, default_value = store::DEFAULT_SCREEN)]
        screen: String,
        /// Show the first boot in a window
        #[arg(long)]
        display: bool,
        /// Say what would be downloaded and installed, and stop there
        #[arg(long)]
        dry_run: bool,
    },
    /// List macOS VMs
    List {
        #[arg(long)]
        json: bool,
    },
    /// Start a VM (at most two run at once), or show a running one's window
    Start {
        name: String,
        /// Show its screen in a window
        #[arg(long)]
        display: bool,
    },
    /// Shut a VM down (stopped hard if it hasn't after a minute)
    Stop {
        name: String,
        /// Stop it right away, without a guest shutdown
        #[arg(long)]
        force: bool,
    },
    /// Put a VM back to its base's clean state (restarted if it was running)
    Reset { name: String },
    /// A copy-on-write copy of a VM, with its own identity; `reset` goes back to the base
    Clone { base: String, name: String },
    /// Save a stopped VM's state as the clean state its clones and resets use
    SetBase { name: String },
    /// Delete a VM and its disk (asks first in a terminal)
    Delete {
        name: String,
        /// Don't ask
        #[arg(short, long)]
        yes: bool,
    },
    /// SSH into a running VM (Remote Login is on), or run a command there
    Shell {
        name: String,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// A VM's settings, state, address and files
    Info {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Run a VM in the foreground (used by `start`)
    #[command(hide = true)]
    Run {
        name: String,
        #[arg(long)]
        display: bool,
    },
}

/// `"mac": true` in ~/.berth/config.json. A local reader until the key
/// joins config.rs.
fn enabled(paths: &Paths) -> bool {
    std::fs::read(paths.config())
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|c| c["mac"] == true)
}

pub fn command(paths: &Paths, cmd: MacCmd) -> Result<()> {
    if !enabled(paths) {
        bail!(
            "macOS VMs are experimental; to try them, add \"mac\": true to {}",
            paths.config().display()
        );
    }
    let root = store::root(paths);
    std::fs::create_dir_all(&root)?;
    match cmd {
        MacCmd::Create { name, ipsw, latest: _, cpus, memory_mib, disk_gib, user, password, screen, display, dry_run } => create(
            paths,
            &name,
            CreateOptions { ipsw, cpus, memory_mib, disk_gib, user, password, screen: Display::parse(&screen)?, display, dry_run },
        ),
        MacCmd::List { json } => list(&root, json),
        MacCmd::Start { name, display } => start_in(&root, &name, display),
        MacCmd::Stop { name, force } => stop_in(&root, &name, force),
        MacCmd::Reset { name } => reset(&root, &name),
        MacCmd::Clone { base, name } => clone(&root, &base, &name),
        MacCmd::SetBase { name } => set_base(&root, &name),
        MacCmd::Delete { name, yes } => delete(&root, &name, yes),
        MacCmd::Shell { name, command } => shell(paths, &root, &name, &command),
        MacCmd::Info { name, json } => info(&root, &name, json),
        MacCmd::Run { name, display } => vz::run(&existing(&root, &name)?, display),
    }
}

// ---- For the gui slice (berth gui sessions and the slot queue) ----

/// The macOS VMs running now (at most MAX_RUNNING).
#[allow(dead_code)]
pub fn running() -> Vec<String> {
    store::running_in(&store::root(&Paths::new()))
}

/// Start a VM as `berth mac start` does: an error, naming the running VMs,
/// when MAX_RUNNING are already up.
#[allow(dead_code)]
pub fn start(name: &str, display: bool) -> Result<()> {
    start_in(&store::root(&Paths::new()), name, display)
}

/// Where a VM's guest agent is reached (each connection to it is one
/// connection to the guest's vsock port 2401).
#[allow(dead_code)]
pub fn agent_socket(name: &str) -> PathBuf {
    Vm::new(&store::root(&Paths::new()), name).agent()
}

/// The VM `set-base` saved last, which throwaway clones start from.
#[allow(dead_code)]
pub fn default_base() -> Option<String> {
    let name = std::fs::read_to_string(default_base_file(&store::root(&Paths::new()))).ok()?;
    let name = name.trim().to_string();
    store::valid_name(&name).is_ok().then_some(name)
}

fn default_base_file(root: &Path) -> PathBuf {
    root.join("default-base")
}

fn existing(root: &Path, name: &str) -> Result<Vm> {
    store::valid_name(name)?;
    let vm = Vm::new(root, name);
    if !vm.exists() {
        bail!("no macOS VM named {name} (berth mac list)");
    }
    Ok(vm)
}

/// Held while deciding whether a VM may start and starting it, so two
/// starts at once can't both be the second VM.
fn start_lock(root: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join(".start.lock"))?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
        bail!("locking {}: {}", root.display(), std::io::Error::last_os_error());
    }
    Ok(file)
}

/// One request to a running VM's process.
fn control(vm: &Vm, req: serde_json::Value) -> Result<serde_json::Value> {
    let mut stream = UnixStream::connect(vm.control()).with_context(|| format!("{} isn't running", vm.name))?;
    stream.set_read_timeout(Some(Duration::from_secs(40)))?;
    writeln!(stream, "{req}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let reply: serde_json::Value = serde_json::from_str(&line).context("bad reply from the VM process")?;
    if reply["ok"] != true {
        bail!("{}: {}", vm.name, reply["error"].as_str().unwrap_or("failed"));
    }
    Ok(reply)
}

fn start_in(root: &Path, name: &str, display: bool) -> Result<()> {
    let vm = existing(root, name)?;
    let _lock = start_lock(root)?;
    if vm.is_running() {
        if display {
            control(&vm, json!({"op": "display"}))?;
        }
        println!("{name} is already running");
        return Ok(());
    }
    store::check_limit(&store::running_in(root), name)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(vm.log())?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["mac", "run", name]);
    if display {
        cmd.arg("--display");
    }
    // Its own process group, so a Ctrl-C here doesn't reach it.
    cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).process_group(0);
    let mut child = cmd.spawn().context("starting the VM process")?;
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(status) = child.try_wait()? {
            bail!("{name} didn't start ({status}); see {}", vm.log().display());
        }
        if let Ok(reply) = control(&vm, json!({"op": "status"}))
            && reply["state"] == "running"
        {
            break;
        }
        if started.elapsed() > Duration::from_secs(60) {
            bail!("{name} didn't start within a minute; see {}", vm.log().display());
        }
    }
    let hint = if display { String::new() } else { format!(" (to see it: berth mac start {name} --display)") };
    println!("Started {name} in {:.1}s{hint}", started.elapsed().as_secs_f64());
    Ok(())
}

fn stop_in(root: &Path, name: &str, force: bool) -> Result<()> {
    let vm = existing(root, name)?;
    if !vm.is_running() {
        println!("{name} is not running");
        return Ok(());
    }
    control(&vm, json!({"op": "stop", "force": force}))?;
    print!("Stopping {name}");
    std::io::stdout().flush()?;
    let deadline = Instant::now() + vz::STOP_GRACE + Duration::from_secs(20);
    let mut ticks = 0u32;
    while vm.is_running() {
        if Instant::now() > deadline {
            println!();
            bail!("{name} is still running; try berth mac stop --force {name}");
        }
        std::thread::sleep(Duration::from_millis(250));
        ticks += 1;
        if ticks.is_multiple_of(4) {
            print!(".");
            std::io::stdout().flush()?;
        }
    }
    println!(" stopped");
    Ok(())
}

struct CreateOptions {
    ipsw: Option<PathBuf>,
    cpus: Option<usize>,
    memory_mib: Option<u64>,
    disk_gib: u64,
    user: String,
    password: String,
    screen: Display,
    display: bool,
    dry_run: bool,
}

fn create(paths: &Paths, name: &str, opts: CreateOptions) -> Result<()> {
    store::valid_name(name)?;
    let root = store::root(paths);
    let vm = Vm::new(&root, name);
    if vm.dir.exists() {
        bail!("{name} already exists (berth mac delete {name} first)");
    }
    if opts.user.is_empty() || !opts.user.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) {
        bail!("--user is a short macOS account name: letters, digits, '_', '-', '.'");
    }
    if opts.disk_gib < 40 {
        bail!("--disk-gib is at least 40 (macOS itself takes ~25 GB)");
    }
    let ipsw = match &opts.ipsw {
        Some(path) => {
            if !path.is_file() {
                bail!("no IPSW at {}", path.display());
            }
            path.canonicalize()?
        }
        None => {
            let image = vz::latest_restore_image()?;
            let file = root.join("cache").join(format!("{}.ipsw", image.build));
            println!("Latest macOS for this Mac: {} ({}), {}", image.version, image.build, image.url);
            match &image.requirements {
                Some((_, cpus, mib)) => println!("It needs at least {cpus} CPUs and {} GiB of memory.", mib / 1024),
                None => bail!("this Mac can't run macOS {} in a VM", image.version),
            }
            if opts.dry_run {
                println!(
                    "Would {} it to {} and install it into {name}.",
                    if file.exists() { "reuse" } else { "download" },
                    file.display()
                );
                return Ok(());
            }
            if !file.exists() {
                download(&image.url, &file)?;
            }
            file
        }
    };
    let image = vz::load_restore_image(&ipsw)?;
    let host_cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let cpus = opts.cpus.unwrap_or(4).max(image.min_cpus).min(host_cpus.max(image.min_cpus));
    let memory_mib = opts.memory_mib.unwrap_or(8192).max(image.min_memory_mib);
    println!(
        "macOS {} ({}) into {name}: {cpus} CPUs, {} GiB memory, {} GiB disk, {} display",
        image.version,
        image.build,
        memory_mib / 1024,
        opts.disk_gib,
        opts.screen
    );
    if opts.dry_run {
        return Ok(());
    }

    // Installing runs the VM, so it counts against the limit too.
    let lock = start_lock(&root)?;
    store::check_limit(&store::running_in(&root), name)?;
    std::fs::create_dir_all(&vm.dir)?;
    let made = (|| -> Result<()> {
        let _pid = vm.lock_pid()?;
        drop(lock);
        let hw = vz::new_hardware(&image.hardware_model);
        vm.save_hw(&hw)?;
        let meta = Meta {
            cpus,
            memory_mib,
            base: None,
            created: store::now(),
            user: Some(opts.user.clone()),
            display: opts.screen,
            build: Some(image.build.clone()),
        };
        vm.save_meta(&meta)?;
        vz::create_aux(&vm, &hw)?;
        create_disk(&vm.disk(), opts.disk_gib)?;
        let started = Instant::now();
        vz::install(&vm, &meta, &hw, &ipsw)?;
        println!("Installed in {:.0}s.", started.elapsed().as_secs_f64());
        // Read by the first boot, which `run` removes once it's started.
        let provision = vz::Provision { full_name: opts.user.clone(), username: opts.user.clone(), password: opts.password.clone() };
        write_private(&vm.provision(), &serde_json::to_vec(&provision)?)?;
        Ok(())
    })();
    if let Err(err) = made {
        let _ = std::fs::remove_dir_all(&vm.dir);
        return Err(err);
    }
    start_in(&root, name, opts.display)?;
    println!(
        "First boot: macOS sets up user {} and logs in (a few minutes). Then:\n  \
         berth mac shell {name}          (password {})\n  \
         berth mac stop {name} && berth mac set-base {name}   to make it the clean state for clones and resets",
        opts.user,
        if opts.password == "berth" { "berth" } else { "as given" }
    );
    Ok(())
}

/// curl, resumable, as provision.rs downloads the Linux image. Apple
/// publishes no checksum for IPSWs; the installer verifies the image.
fn download(url: &str, file: &Path) -> Result<()> {
    std::fs::create_dir_all(file.parent().unwrap())?;
    let part = file.with_extension("ipsw.part");
    println!("Downloading {url} (about 15 GB, once)…");
    let status = Command::new("curl")
        .args(["--fail", "--location", "--progress-bar", "--continue-at", "-", "-o"])
        .arg(&part)
        .arg(url)
        .status()
        .context("running curl")?;
    if !status.success() {
        bail!("downloading {url} failed ({status}); run the same command again to resume");
    }
    std::fs::rename(&part, file)?;
    Ok(())
}

/// The VM's disk: ASIF on macOS 26+ (see provision.rs), else a sparse raw
/// file. Either takes space only as the guest writes.
fn create_disk(disk: &Path, gib: u64) -> Result<()> {
    if asif_supported() {
        let status = Command::new("diskutil")
            .args(["image", "create", "blank", "--fs", "None", "--format", "ASIF", "--size", &format!("{gib}G")])
            .arg(disk)
            .stdout(Stdio::null())
            .status()
            .context("running diskutil")?;
        if !status.success() {
            bail!("diskutil image create failed ({status})");
        }
        return Ok(());
    }
    std::fs::File::create(disk)?.set_len(gib << 30)?;
    Ok(())
}

fn asif_supported() -> bool {
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().split('.').next()?.parse::<u32>().ok())
        .is_some_and(|major| major >= 26)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// The disk and aux files a VM's clones and resets come from: its saved
/// clean state, or its current files if it's stopped.
fn source_of(vm: &Vm) -> Result<(PathBuf, PathBuf, &'static str)> {
    if vm.has_clean() {
        return Ok((vm.clean().join("disk.img"), vm.clean().join("aux.img"), "its saved clean state"));
    }
    if vm.is_running() {
        bail!("{0} is running and has no saved clean state: berth mac stop {0} && berth mac set-base {0}", vm.name);
    }
    Ok((vm.disk(), vm.aux(), "its current disk"))
}

fn clone(root: &Path, base: &str, name: &str) -> Result<()> {
    let base_vm = existing(root, base)?;
    store::valid_name(name)?;
    let vm = Vm::new(root, name);
    if vm.dir.exists() {
        bail!("{name} already exists");
    }
    let (disk, aux, what) = source_of(&base_vm)?;
    let started = Instant::now();
    std::fs::create_dir_all(&vm.dir)?;
    let made = (|| -> Result<()> {
        store::clone_file(&disk, &vm.disk())?;
        store::clone_file(&aux, &vm.aux())?;
        // Same virtual hardware, but its own identity: two Macs with one
        // machine identifier can't both run, and would share a DHCP lease.
        let hw = vz::new_hardware(&base_vm.load_hw()?.hardware_model_bytes()?);
        vm.save_hw(&hw)?;
        let meta = Meta { base: Some(base.to_string()), created: store::now(), ..base_vm.load_meta()? };
        vm.save_meta(&meta)
    })();
    if let Err(err) = made {
        let _ = std::fs::remove_dir_all(&vm.dir);
        return Err(err);
    }
    println!(
        "Cloned {base} ({what}) to {name} in {:.1}s; it shares {base}'s blocks until either changes. Start it: berth mac start {name}",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn set_base(root: &Path, name: &str) -> Result<()> {
    let vm = existing(root, name)?;
    if vm.is_running() {
        bail!("{name} is running; stop it first: berth mac stop {name}");
    }
    std::fs::create_dir_all(vm.clean())?;
    store::clone_file(&vm.disk(), &vm.clean().join("disk.img"))?;
    store::clone_file(&vm.aux(), &vm.clean().join("aux.img"))?;
    std::fs::write(default_base_file(root), name)?;
    println!("{name}'s current state is now its clean state: clones start from it and `berth mac reset` goes back to it.");
    Ok(())
}

fn reset(root: &Path, name: &str) -> Result<()> {
    let vm = existing(root, name)?;
    let meta = vm.load_meta()?;
    let (disk, aux, from) = match &meta.base {
        Some(base) => {
            let base_vm = Vm::new(root, base);
            if !base_vm.exists() {
                bail!("{name}'s base {base} was deleted; there's nothing to reset to");
            }
            let (disk, aux, _) = source_of(&base_vm)?;
            (disk, aux, base.clone())
        }
        None if vm.has_clean() => (vm.clean().join("disk.img"), vm.clean().join("aux.img"), "its clean state".to_string()),
        None => bail!("{name} has no base to reset to; once it's set up, save one: berth mac stop {name} && berth mac set-base {name}"),
    };
    let started = Instant::now();
    let was_running = vm.is_running();
    if was_running {
        // Its disk is about to be replaced: no need for a clean shutdown.
        stop_in(root, name, true)?;
    }
    // The identity (hw.json) stays the VM's own.
    store::clone_file(&disk, &vm.disk())?;
    store::clone_file(&aux, &vm.aux())?;
    let _ = std::fs::remove_file(vm.provision());
    println!("Reset {name} to {from} in {:.1}s.", started.elapsed().as_secs_f64());
    if was_running {
        start_in(root, name, false)?;
    }
    Ok(())
}

fn delete(root: &Path, name: &str, yes: bool) -> Result<()> {
    let vm = existing(root, name)?;
    if !yes && std::io::stdin().is_terminal() {
        print!("Delete macOS VM {name} and its disk? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            bail!("not deleted");
        }
    }
    if vm.is_running() {
        stop_in(root, name, true)?;
    }
    let clones: Vec<String> = store::all(root)
        .into_iter()
        .filter(|other| other.load_meta().is_ok_and(|m| m.base.as_deref() == Some(name)))
        .map(|other| other.name)
        .collect();
    std::fs::remove_dir_all(&vm.dir)?;
    if std::fs::read_to_string(default_base_file(root)).is_ok_and(|b| b.trim() == name) {
        let _ = std::fs::remove_file(default_base_file(root));
    }
    println!("Deleted {name}.");
    if !clones.is_empty() {
        println!("Its clones keep working but can't be reset any more: {}", clones.join(", "));
    }
    Ok(())
}

/// What `list` and `info` show about a VM.
fn describe(vm: &Vm) -> Result<serde_json::Value> {
    let meta = vm.load_meta()?;
    let hw = vm.load_hw().ok();
    let pid = vm.running_pid();
    let ip = pid.and(hw.as_ref()).and_then(|hw| dhcp::lookup(&hw.mac_address));
    Ok(json!({
        "name": vm.name,
        "state": if pid.is_some() { "running" } else { "stopped" },
        "pid": pid,
        "cpus": meta.cpus,
        "memory_mib": meta.memory_mib,
        "base": meta.base,
        "clean_state": vm.has_clean(),
        "created": meta.created,
        "build": meta.build,
        "user": meta.user,
        "screen": meta.display.to_string(),
        "display": meta.display,
        "mac_address": hw.map(|h| h.mac_address),
        "ip": ip.map(|ip| ip.to_string()),
        "disk_bytes": store::allocated(&vm.disk()),
        "dir": vm.dir,
        "control": vm.control(),
        "agent": vm.agent(),
        "push": vm.push(),
        "log": vm.log(),
    }))
}

fn list(root: &Path, as_json: bool) -> Result<()> {
    let vms: Vec<serde_json::Value> = store::all(root).iter().filter_map(|vm| describe(vm).ok()).collect();
    if as_json {
        println!("{}", serde_json::to_string_pretty(&vms)?);
        return Ok(());
    }
    if vms.is_empty() {
        println!("No macOS VMs. Make one with: berth mac create <name>");
        return Ok(());
    }
    println!("{:<20} {:<8} {:>4} {:>7} {:>8}  {:<16} IP", "NAME", "STATE", "CPUS", "MEMORY", "DISK", "BASE");
    for v in &vms {
        let base = match (v["base"].as_str(), v["clean_state"] == true) {
            (Some(b), _) => format!("clone of {b}"),
            (None, true) => "base".to_string(),
            (None, false) => "-".to_string(),
        };
        println!(
            "{:<20} {:<8} {:>4} {:>5}Gi {:>6.1}G  {:<16} {}",
            v["name"].as_str().unwrap_or_default(),
            v["state"].as_str().unwrap_or_default(),
            v["cpus"].as_u64().unwrap_or(0),
            v["memory_mib"].as_u64().unwrap_or(0) / 1024,
            v["disk_bytes"].as_u64().unwrap_or(0) as f64 / (1u64 << 30) as f64,
            base,
            v["ip"].as_str().unwrap_or("-"),
        );
    }
    Ok(())
}

fn info(root: &Path, name: &str, as_json: bool) -> Result<()> {
    let vm = existing(root, name)?;
    let v = describe(&vm)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let s = |k: &str| v[k].as_str().map_or("-".to_string(), str::to_string);
    println!("Name:     {name}");
    println!("State:    {}{}", s("state"), v["pid"].as_u64().map_or(String::new(), |p| format!(" (pid {p})")));
    println!("macOS:    {}", s("build"));
    println!("CPUs:     {}", v["cpus"]);
    println!("Memory:   {} MiB", v["memory_mib"]);
    println!("Screen:   {}", s("screen"));
    println!("Disk:     {:.1} GiB used", v["disk_bytes"].as_u64().unwrap_or(0) as f64 / (1u64 << 30) as f64);
    println!("Base:     {}{}", s("base"), if v["clean_state"] == true { " (has a saved clean state)" } else { "" });
    println!("User:     {}", s("user"));
    println!("MAC:      {}", s("mac_address"));
    println!("IP:       {}", s("ip"));
    println!("Files:    {}", s("dir"));
    println!("Push:     {} (in the guest: /Volumes/My Shared Files/{})", s("push"), vz::PUSH_SHARE);
    println!("Log:      {}", s("log"));
    Ok(())
}

fn shell(paths: &Paths, root: &Path, name: &str, command: &[String]) -> Result<()> {
    let vm = existing(root, name)?;
    if !vm.is_running() {
        bail!("{name} isn't running: berth mac start {name}");
    }
    let meta = vm.load_meta()?;
    let hw = vm.load_hw()?;
    let ip = dhcp::lookup(&hw.mac_address)
        .with_context(|| format!("{name} has no address yet (no DHCP lease for {} in {}); is it still booting?", hw.mac_address, dhcp::LEASES))?;
    let user = meta.user.unwrap_or_else(|| "berth".into());
    let mut ssh = Command::new("ssh");
    // Every clone and reset is a new host key at a reused address.
    ssh.args(["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]);
    // berth's key, for a guest it was added to (else the password).
    if paths.ssh_key().exists() {
        ssh.arg("-i").arg(paths.ssh_key());
    }
    ssh.arg(format!("{user}@{ip}"));
    if command.is_empty() {
        ssh.arg("-t");
    }
    ssh.args(command);
    Err(ssh.exec().into())
}
