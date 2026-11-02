//! berth-gui-mac in a macOS VM: installing the copy built into berth
//! (guest/mac-agent, embedded by build.rs) and walking a person through its
//! one-time Accessibility and Screen Recording grants, which macOS only
//! takes from someone clicking in System Settings. Clones and agent updates
//! keep the grants (docs/LEARNINGS.md, macOS VMs), so this is once per base.
//!
//! It relies on mod.rs's SSH helpers (berth's key, installed by create) and
//! the VM process's agent.sock (docs/gui-protocol.md).

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::store::Vm;
use super::{control, ssh_target, ssh_to};
use crate::paths::Paths;

/// Empty when berth was built without it (no Swift toolchain).
const BINARY: &[u8] = include_bytes!(env!("BERTH_GUI_MAC"));
const INSTALL: &str = include_str!("../../guest/mac-agent/install.sh");
const PLIST: &str = include_str!("../../guest/mac-agent/dev.berth.gui-mac.plist");
/// Where the files go in the push share, and where the guest sees them.
const STAGING: &str = ".berth-gui-mac";
const GUEST_PUSH: &str = "/Volumes/My Shared Files/berth-push";

const ACCESSIBILITY: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const SCREEN_RECORDING: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

pub fn bundled() -> bool {
    !BINARY.is_empty()
}

/// Install (or update) the agent through its install.sh, as the VM's user.
pub fn install(paths: &Paths, vm: &Vm) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !bundled() {
        bail!("this berth was built without berth-gui-mac (scripts/build.sh builds it with Swift)");
    }
    let dir = vm.push().join(STAGING);
    std::fs::create_dir_all(&dir)?;
    for (name, bytes, mode) in [
        ("berth-gui-mac", BINARY, 0o755),
        ("install.sh", INSTALL.as_bytes(), 0o755),
        ("dev.berth.gui-mac.plist", PLIST.as_bytes(), 0o644),
    ] {
        std::fs::write(dir.join(name), bytes)?;
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(mode))?;
    }
    let target = ssh_target(vm)?;
    let status = ssh_to(paths, vm)
        .args(["-o", "BatchMode=yes", &target])
        .arg(format!("cd '{GUEST_PUSH}/{STAGING}' && BERTH_NO_PANES=1 ./install.sh ./berth-gui-mac"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .context("running ssh")?;
    let _ = std::fs::remove_dir_all(&dir);
    if !status.success() {
        bail!("installing berth-gui-mac in {} failed ({status})", vm.name);
    }
    // launchd starts it; wait until it answers.
    let deadline = Instant::now() + Duration::from_secs(20);
    while call(vm, "hello", json!({})).is_err() {
        if Instant::now() > deadline {
            bail!("berth-gui-mac didn't answer in {} after installing it", vm.name);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// After create: the grants in a terminal, else how to do them later.
pub fn after_create(paths: &Paths, vm: &Vm) -> Result<()> {
    if !bundled() {
        println!("berth-gui-mac isn't built into this berth: see guest/mac-agent/README.md to install it.");
        return Ok(());
    }
    install(paths, vm)?;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        grant(paths, vm)
    } else {
        println!("berth-gui-mac is installed. Give it its grants once, in a terminal: berth mac grant {}", vm.name);
        Ok(())
    }
}

/// `berth mac grant`: show the VM, open each privacy pane berth-gui-mac
/// still needs, and wait for the switch.
pub fn grant(paths: &Paths, vm: &Vm) -> Result<()> {
    if call(vm, "hello", json!({})).is_err() {
        println!("Installing berth-gui-mac in {}…", vm.name);
        install(paths, vm)?;
    }
    let (mut ax, mut screen) = permissions(vm)?;
    if ax && screen {
        println!("berth-gui-mac in {} has Accessibility and Screen Recording.", vm.name);
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("the grants need a person: run berth mac grant {} in a terminal", vm.name);
    }
    control(vm, json!({"op": "display"}))?;
    let password = std::fs::read_to_string(vm.password()).ok().filter(|_| vm.load_meta().is_ok_and(|m| m.user.is_some()));
    let asks = match password.as_deref() {
        Some(p) if p == super::DEFAULT_PASSWORD => format!("\n   If it asks for a password, it's {p}."),
        Some(_) => format!("\n   If it asks for a password, it's in {}.", vm.password().display()),
        None => String::new(),
    };
    println!("{}'s window is open. berth-gui-mac needs two switches, once for this VM and its clones.", vm.name);
    while !ax {
        open(vm, ACCESSIBILITY)?;
        println!("\n1. In the VM: System Settings > Privacy & Security > Accessibility: switch on berth-gui-mac.{asks}");
        wait_for_enter()?;
        (ax, screen) = permissions(vm)?;
        if !ax {
            println!("Accessibility still isn't on for berth-gui-mac.");
        }
    }
    while !screen {
        open(vm, SCREEN_RECORDING)?;
        // The pane lists the agent only once it tries a capture with the
        // pane open (one tried earlier, before the pane, didn't show up).
        let _ = call(vm, "screenshot", json!({"format": "png"}));
        println!("\n2. In the VM: Screen & System Audio Recording: switch on berth-gui-mac (if macOS offers to quit it, choose Later).{asks}");
        wait_for_enter()?;
        // Screen Recording applies to a process started after the grant.
        restart(paths, vm)?;
        screen = permissions(vm)?.1;
        if !screen {
            println!("Screen Recording still isn't on for berth-gui-mac.");
        }
    }
    let _ = call(vm, "quit", json!({"app": "System Settings"}));
    println!("berth-gui-mac in {} has Accessibility and Screen Recording.", vm.name);
    Ok(())
}

fn permissions(vm: &Vm) -> Result<(bool, bool)> {
    let hello = call(vm, "hello", json!({}))?;
    let p = &hello["permissions"];
    Ok((p["accessibility"] == true, p["screen_recording"] == true))
}

fn open(vm: &Vm, url: &str) -> Result<()> {
    call(vm, "open", json!({"app": url, "wait_s": 15})).map(|_| ())
}

fn restart(paths: &Paths, vm: &Vm) -> Result<()> {
    let target = ssh_target(vm)?;
    ssh_to(paths, vm)
        .args(["-o", "BatchMode=yes", &target, "launchctl kickstart -k gui/$(id -u)/dev.berth.gui-mac"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .context("running ssh")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while call(vm, "hello", json!({})).is_err() {
        if Instant::now() > deadline {
            bail!("berth-gui-mac didn't come back in {}", vm.name);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}

fn wait_for_enter() -> Result<()> {
    print!("   Press Enter when it's on: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        bail!("stopped before the grants were given; run berth mac grant again");
    }
    Ok(())
}

/// One request to the guest agent through the VM process's agent.sock; a
/// payload (a screenshot) is left unread.
fn call(vm: &Vm, verb: &str, args: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(vm.agent()).with_context(|| format!("{} isn't running", vm.name))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    writeln!(stream, "{}", json!({"v": 1, "verb": verb, "args": args}))?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let reply: Value = serde_json::from_str(&line).context("bad reply from berth-gui-mac")?;
    if reply["ok"] != true {
        bail!("{}", reply["error"].as_str().unwrap_or("berth-gui-mac failed"));
    }
    Ok(reply["result"].clone())
}
