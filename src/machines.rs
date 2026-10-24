//! `berth machine`: Linux machines (full distros with systemd) as
//! systemd-nspawn containers in the VM; see agent/src/machines.rs.

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

pub fn create(paths: &Paths, name: &str, distro: &str, arch: &str, cloud_init: Option<&std::path::Path>, iso: Isolation) -> Result<()> {
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
    println!("Creating {name} ({distro_name}, {})...", notes.join(", "));
    let flag = |b: bool| if b { "1" } else { "" };
    call(
        paths,
        "create",
        &[name, distro, &user, &uid, &home, arch, &user_data, flag(iso.isolated), flag(iso.network), &mounts.join("\n")],
    )?;
    if !user_data.is_empty() {
        println!("cloud-init runs on this first boot: berth machine shell {name} -- cloud-init status --wait");
    }
    println!("Machine {name} is running. Open a shell: berth machine shell {name}");
    println!("Domain: {name}.berth.local");
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
    let owner = format!("{}:{}", unsafe { libc::getuid() }, unsafe { libc::getgid() });
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
    let mut script = format!("exec nsenter -t \"$(machinectl show {name} -p Leader --value)\" -a -- su -l {user}");
    if !command.is_empty() {
        let inner = command.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
        script.push_str(&format!(" -c {}", quote(&inner)));
    }
    // ssh hands the joined arguments to the VM's shell: quote the script once.
    Ok(vec!["sudo".into(), "sh".into(), "-c".into(), quote(&script)])
}

#[cfg(test)]
mod tests {
    use super::*;

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
