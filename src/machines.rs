//! `berth machine`: Linux machines (full distros with systemd) as
//! systemd-nspawn containers in the VM; see agent/src/machines.rs.

use std::time::Duration;

use anyhow::{Result, bail};
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

pub fn create(paths: &Paths, name: &str, distro: &str) -> Result<()> {
    let user = std::env::var("USER")?;
    let uid = unsafe { libc::getuid() }.to_string();
    let home = paths.home.to_string_lossy();
    println!("Creating {name} ({})...", if distro.is_empty() { "ubuntu:noble" } else { distro });
    call(paths, "create", &[name, distro, &user, &uid, &home])?;
    println!("Machine {name} is running. Open a shell: berth machine shell {name}");
    println!("Domain: {name}.berth.local");
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
