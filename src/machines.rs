//! `berth machine`: Linux machines (full distros with systemd) as
//! systemd-nspawn containers in the VM; see agent/src/machines.rs.
//!
//! With `"gui": true` in config.json, also GUI machines (`create --gui`) and
//! their headless app sessions (`run`, `display`), through the agent's GUI
//! module (agent/src/gui/, docs/gui-protocol.md).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::daemon::{MachineReply, Request};
use crate::paths::Paths;

#[derive(Deserialize)]
struct Machine {
    name: String,
    state: String,
    os: String,
    addresses: Vec<String>,
}

fn call(paths: &Paths, action: &str, args: &[&str]) -> Result<String> {
    let request = Request::Machine { action: action.into(), args: args.iter().map(|s| s.to_string()).collect() };
    let reply: MachineReply = crate::request_with_timeout(paths, &request, Duration::from_secs(900))?;
    match reply.output.strip_prefix("error: ") {
        Some(err) => bail!("{err}"),
        None => Ok(reply.output),
    }
}

/// How a new machine is set apart from the Mac.
pub struct Isolation {
    pub isolated: bool,
    pub network: bool,
    pub mounts: Vec<std::path::PathBuf>,
}

pub fn create(
    paths: &Paths,
    name: &str,
    distro: &str,
    arch: &str,
    cloud_init: Option<&std::path::Path>,
    iso: Isolation,
    gui: Option<&str>,
) -> Result<()> {
    if let Some(kind) = gui {
        require_gui(paths)?;
        if !matches!(kind, "1" | "omarchy") {
            bail!("--gui {kind}: plain --gui, or --gui omarchy");
        }
    }
    let user = std::env::var("USER")?;
    let uid = unsafe { libc::getuid() }.to_string();
    let home = paths.home.to_string_lossy();
    let user_data = match cloud_init {
        Some(path) => {
            let data = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            if !data.starts_with("#cloud-config") && !data.starts_with("#!") {
                bail!("{}: cloud-init user data starts with #cloud-config (or #! for a script)", path.display());
            }
            data
        }
        None => String::new(),
    };
    let mut mounts = Vec::new();
    for m in &iso.mounts {
        mounts.push(vm_path(paths, m)?);
    }
    let distro_name = if distro.is_empty() { "ubuntu:noble" } else { distro };
    let mut notes = vec![arch.to_string()];
    if !user_data.is_empty() {
        notes.push("cloud-init".into());
    }
    if iso.isolated {
        notes.push(if iso.network { "isolated, network too".into() } else { "isolated".into() });
    }
    match gui {
        Some("omarchy") => notes.push("GUI, Omarchy".into()),
        Some(_) => notes.push("GUI".into()),
        None => {}
    }
    println!("Creating {name} ({distro_name}, {})...", notes.join(", "));
    let flag = |b: bool| if b { "1" } else { "" };
    let mounts = mounts.join("\n");
    let mut args = vec![name, distro, &user, &uid, &home, arch, &user_data, flag(iso.isolated), flag(iso.network), &mounts];
    // Only --gui machines send the extra argument: everything else goes to
    // the agent exactly as before.
    if let Some(kind) = gui {
        args.push(kind);
    }
    call(paths, "create", &args)?;
    if !user_data.is_empty() {
        println!("cloud-init runs on this first boot: berth machine shell {name} -- cloud-init status --wait");
    }
    println!("Machine {name} is running. Open a shell: berth machine shell {name}");
    println!("Domain: {name}.berth.local");
    if gui.is_some() {
        println!("Run an app in it: berth machine run {name} -- <command>");
    }
    Ok(())
}

/// Linux GUI sessions are behind `"gui": true` in config.json until
/// they're stable (docs/plans/gui-vms-agent-testing.md).
fn gui_enabled(paths: &Paths) -> bool {
    std::fs::read(paths.config())
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|c| c.get("gui").and_then(|v| v.as_bool()) == Some(true))
}

fn require_gui(paths: &Paths) -> Result<()> {
    if !gui_enabled(paths) {
        bail!("Linux GUI sessions are experimental: turn them on with \"gui\": true in ~/.berth/config.json");
    }
    Ok(())
}

/// One request to the machine's GUI agent (docs/gui-protocol.md) through
/// the `machine gui` action: its reply, and a payload (a screenshot) if any.
fn gui_call(paths: &Paths, name: &str, session: Option<&str>, verb: &str, args: serde_json::Value) -> Result<(serde_json::Value, Vec<u8>)> {
    let request = serde_json::json!({"v": 1, "session": session, "verb": verb, "args": args});
    let output = call(paths, "gui", &[name, &request.to_string()]).map_err(|e| {
        if e.to_string().contains("unknown action gui") {
            anyhow::anyhow!("the VM's berth agent is older than GUI sessions; restart berth to update it")
        } else {
            e
        }
    })?;
    parse_gui_output(&output)
}

/// The action's output: the reply line, then the payload as a base64 line.
fn parse_gui_output(output: &str) -> Result<(serde_json::Value, Vec<u8>)> {
    let mut lines = output.lines();
    let reply: serde_json::Value = serde_json::from_str(lines.next().unwrap_or("")).context("the GUI agent's reply")?;
    if reply["ok"].as_bool() != Some(true) {
        bail!("{}", reply["error"].as_str().unwrap_or("the GUI request failed"));
    }
    let payload = match lines.next() {
        Some(b64) => base64_decode(b64.trim()).context("the GUI agent's payload")?,
        None => Vec::new(),
    };
    Ok((reply["result"].clone(), payload))
}

/// Standard base64, as the agent writes payloads (agent/src/gui/mod.rs).
fn base64_decode(text: &str) -> Result<Vec<u8>> {
    let value = |c: u8| -> Result<u32> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("bad base64"),
        } as u32)
    };
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        bail!("bad base64 length");
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = (n << 6) | value(c)?;
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Ok(out)
}

/// `berth machine run <name> -- <command>`: the command in an app session
/// of its own (cage, one window); prints the session id.
pub fn run(paths: &Paths, name: &str, command: &[String]) -> Result<()> {
    require_gui(paths)?;
    if command.is_empty() {
        bail!("give a command: berth machine run {name} -- <command>");
    }
    let (result, _) = gui_call(paths, name, None, "session.new", serde_json::json!({"agent": "terminal", "app": command}))?;
    let session = result["session"].as_str().context("no session id in the reply")?;
    eprintln!("{} runs in {name}, session {session}. Its screen: berth machine display {name} --session {session}", command[0]);
    println!("{session}");
    Ok(())
}

/// `berth machine display <name>`: the machine's GUI sessions, and a
/// screenshot of one on request. A Mac window showing a session live comes
/// with berth's GPU display; until then this says so rather than pretend.
pub fn display(paths: &Paths, name: &str, session: Option<&str>, screenshot: Option<&std::path::Path>) -> Result<()> {
    require_gui(paths)?;
    if let Some(file) = screenshot {
        let (result, png) = gui_call(paths, name, session, "screenshot", serde_json::json!({}))?;
        std::fs::write(file, &png).with_context(|| file.display().to_string())?;
        println!("Saved {}x{} screenshot to {}", result["width"], result["height"], file.display());
        return Ok(());
    }
    let (result, _) = gui_call(paths, name, None, "session.list", serde_json::json!({}))?;
    let sessions = result["sessions"].as_array().cloned().unwrap_or_default();
    if sessions.is_empty() {
        println!("No GUI sessions in {name}. Start an app: berth machine run {name} -- <command>");
    } else {
        for s in &sessions {
            let apps: Vec<&str> = s["apps"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect();
            let size = s["size"].as_array().map(|v| format!("{}x{}", v[0], v.get(1).unwrap_or(&serde_json::Value::Null))).unwrap_or_default();
            println!("{:10} {:6} {:10} {:22} {}", s["session"].as_str().unwrap_or(""), s["kind"].as_str().unwrap_or(""), size, s["since"].as_str().unwrap_or(""), apps.join(", "));
        }
    }
    println!();
    println!("Sessions run headless: berth can't show one in a Mac window yet (that comes with GPU display support).");
    println!("To see one now, save its screen: berth machine display {name} --session <id> --screenshot screen.png");
    Ok(())
}

/// A Mac path as the VM sees it: the same path, in a folder it shares
/// (home, /Volumes, /tmp, $TMPDIR).
fn vm_path(paths: &Paths, path: &std::path::Path) -> Result<String> {
    let real = std::fs::canonicalize(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let shared = std::iter::once(paths.home.clone()).chain(crate::virtiofs::EXTRA_SHARES.iter().map(|(_, p)| p.into()));
    for root in shared {
        if real.starts_with(&root) {
            return Ok(real.to_string_lossy().into_owned());
        }
    }
    bail!("{}: not in a folder berth shares with the VM (your home, /Volumes, /tmp)", path.display())
}

pub fn rename(paths: &Paths, name: &str, new: &str) -> Result<()> {
    call(paths, "rename", &[name, new])?;
    println!("Renamed {name} to {new}: {new}.berth.local, ssh {new}.berth");
    Ok(())
}

pub fn info(paths: &Paths, name: &str) -> Result<()> {
    let info: serde_json::Value = serde_json::from_str(&call(paths, "info", &[name])?)?;
    let s = |k: &str| info[k].as_str().unwrap_or("").to_string();
    println!("Name:      {}", s("name"));
    println!("State:     {}", s("state"));
    println!("OS:        {} ({})", s("os"), s("arch"));
    let addresses: Vec<&str> = info["addresses"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect();
    println!("Address:   {}  ({name}.berth.local)", if addresses.is_empty() { "–".to_string() } else { addresses.join(", ") });
    println!("Disk:      {}", s("disk"));
    let mounts: Vec<&str> = info["mounts"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect();
    let isolation = match (info["isolated"].as_bool(), info["isolated_network"].as_bool()) {
        (_, Some(true)) => "isolated, network too (DNS and the internet only)",
        (Some(true), _) => "isolated (no home folder, mac command or SSH agent)",
        _ => "integrated (home folder, mac command, SSH agent)",
    };
    println!("Mac:       {isolation}");
    println!("Folders:   {}", if mounts.is_empty() { "none".to_string() } else { mounts.join(", ") });
    println!("Autostart: {}", if info["autostart"].as_bool() == Some(true) { "yes" } else { "no" });
    Ok(())
}

pub fn logs(paths: &Paths, name: &str, lines: u32) -> Result<()> {
    print!("{}", call(paths, "logs", &[name, &lines.to_string()])?);
    Ok(())
}

pub fn push(paths: &Paths, name: &str, from: &std::path::Path, to: Option<&str>) -> Result<()> {
    let source = vm_path(paths, from)?;
    let user = std::env::var("USER")?;
    let base = std::path::Path::new(&source).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let target = to.map(str::to_string).unwrap_or_else(|| format!("/home/{user}/{base}"));
    // "uid:": the user's own group in the machine (the Mac's gid can mean
    // something else there: staff, 20, is Debian's dialout).
    let owner = format!("{}:", unsafe { libc::getuid() });
    call(paths, "copy-to", &[name, &source, &target, &owner])?;
    println!("Copied {} to {name}:{target}", from.display());
    Ok(())
}

pub fn pull(paths: &Paths, name: &str, from: &str, to: Option<&std::path::Path>) -> Result<()> {
    let base = std::path::Path::new(from).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let dest = match to {
        Some(t) if t.is_dir() => t.join(&base),
        Some(t) => t.to_path_buf(),
        None => std::env::current_dir()?.join(&base),
    };
    let parent = dest.parent().context("destination folder")?;
    let target = std::path::Path::new(&vm_path(paths, parent)?).join(dest.file_name().context("destination name")?);
    call(paths, "copy-from", &[name, from, &target.to_string_lossy(), ""])?;
    println!("Copied {name}:{from} to {}", dest.display());
    Ok(())
}

pub fn list(paths: &Paths) -> Result<()> {
    let machines: Vec<Machine> = serde_json::from_str(&call(paths, "list", &[])?)?;
    if machines.is_empty() {
        println!("No machines. Create one: berth machine create <name> [--distro ubuntu:noble]");
        return Ok(());
    }
    for m in machines {
        println!("{:14} {:8} {:16} {}", m.name, m.state, m.addresses.first().map_or("", String::as_str), m.os);
    }
    Ok(())
}

pub fn simple(paths: &Paths, action: &str, name: &str) -> Result<()> {
    call(paths, action, &[name]).map(|_| ())
}

/// The command `berth shell` runs in the VM to enter a machine as the user:
/// nsenter into the machine's init process, then `su -l`. Unlike machinectl
/// shell or systemd-run this works for any init system (Alpine's OpenRC too),
/// doesn't need the machine's D-Bus, and works with or without a terminal.
pub fn shell_command(name: &str, command: &[String]) -> Result<Vec<String>> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        bail!("bad machine name");
    }
    let user = std::env::var("USER")?;
    let quote = |w: &str| format!("'{}'", w.replace('\'', "'\\''"));
    let inner = command.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
    // An isolated machine through its own systemd (`machinectl shell`),
    // which keeps the machine's limits: nsenter would run the machine's su
    // as the VM's unconfined root (agent/src/machines.rs, run_in).
    // (`machinectl shell` drops a command's output without a terminal:
    // systemd-run, which is confined the same way, for commands.)
    let confined = if command.is_empty() {
        format!("machinectl -q shell {user}@{name}")
    } else {
        format!(
            "systemd-run -M {name} --uid={user} --wait --quiet --collect -p WorkingDirectory=~ \"$([ -t 0 ] && echo --pty || echo --pipe)\" /bin/sh -lc {}",
            quote(&inner)
        )
    };
    let mut direct = format!("nsenter -t \"$(machinectl show {name} -p Leader --value)\" -a -- su -l {user}");
    if !command.is_empty() {
        direct.push_str(&format!(" -c {}", quote(&inner)));
    }
    let script = format!("if grep -qx '# berth: isolated' /etc/systemd/nspawn/{name}.nspawn 2>/dev/null; then exec {confined}; else exec {direct}; fi");
    // ssh hands the joined arguments to the VM's shell: quote the script once.
    Ok(vec!["sudo".into(), "sh".into(), "-c".into(), quote(&script)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_payloads() {
        for (raw, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64_decode(enc).unwrap(), raw.as_bytes());
        }
        assert_eq!(base64_decode("iVBORwD/gA==").unwrap(), [0x89u8, b'P', b'N', b'G', 0, 255, 128]);
        assert!(base64_decode("abc").is_err());
        assert!(base64_decode("ab!d").is_err());
    }

    #[test]
    fn gui_replies() {
        // As agent/src/gui/mod.rs's machine_action writes them.
        let (result, payload) = parse_gui_output("{\"ok\":true,\"result\":{\"width\":2,\"height\":1},\"error\":null,\"payload_len\":3}\nZm9v\n").unwrap();
        assert_eq!((result["width"].as_u64(), payload.as_slice()), (Some(2), b"foo".as_slice()));
        let (result, payload) = parse_gui_output("{\"ok\":true,\"result\":{\"session\":\"s-1\"},\"error\":null,\"payload_len\":0}\n").unwrap();
        assert_eq!((result["session"].as_str(), payload.len()), (Some("s-1"), 0));
        let err = parse_gui_output("{\"ok\":false,\"result\":{},\"error\":\"no machine x\",\"payload_len\":0}\n").unwrap_err();
        assert_eq!(err.to_string(), "no machine x");
    }

    #[test]
    fn shell_quoting_survives_every_shell() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: only this test reads USER.
        unsafe { std::env::set_var("USER", "dev") };
        // A user command with a quote and a variable that must expand in the
        // machine (as `berth machine shell ubu sh -c 'echo "it'\''s $HOME"'`).
        let remote = shell_command("ubu", &["sh".into(), "-c".into(), "echo \"it's $HOME\"".into()]).unwrap();

        // Stub the VM's tools as executables so the line runs exactly as the
        // VM's shell would run it (ssh sends the joined line): sudo and
        // nsenter pass through, su runs its -c string with HOME=/x.
        let bin = std::env::temp_dir().join(format!("berth-shell-test-{}", std::process::id()));
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in [
            ("sudo", "exec \"$@\""),
            ("machinectl", "echo 1"),
            ("nsenter", "shift 4; exec \"$@\""),
            ("su", "HOME=/x exec /bin/sh -c \"$4\""),
        ] {
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &remote.join(" ")])
            .env("PATH", path)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&bin).ok();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "it's /x", "{}", String::from_utf8_lossy(&out.stderr));
    }
}
