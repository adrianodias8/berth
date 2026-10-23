//! `berth engine`: Podman and nerdctl next to Docker (agent/src/engines.rs).
//!
//! Podman's API socket is bridged to ~/.berth/run/podman.sock and the macOS
//! podman-remote client is installed with a default `berth` connection.
//! nerdctl only exists for Linux, so ~/.berth/bin/nerdctl is a wrapper that
//! runs it in the VM from the same directory (home paths are identical there).

use std::fs;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::daemon::{EngineReply, Request};
use crate::paths::Paths;

/// Matches Debian 13's podman, which serves the API: remote clients should
/// not be newer than the server.
const PODMAN_REMOTE: &str = "v5.4.2";
const PODMAN_REMOTE_SHA256: &str = "cd87d2285a75a65edb39af3f1fbc1d830c69ea8e62d6bedfe60639ba05b2f014";

fn call(paths: &Paths, args: &str) -> Result<String> {
    let reply: EngineReply =
        crate::request_with_timeout(paths, &Request::Engine { args: args.into() }, Duration::from_secs(900))?;
    match reply.output.strip_prefix("error: ") {
        Some(err) => bail!("{err}"),
        None => Ok(reply.output),
    }
}

pub fn enable(paths: &Paths, name: &str) -> Result<()> {
    match name {
        "podman" => {
            println!("Installing Podman in the VM...");
            call(paths, "enable podman")?;
            install_podman_remote(paths)?;
            println!("Podman is ready: `podman ps` (connection 'berth', socket {})", paths.podman_socket().display());
        }
        "nerdctl" => {
            println!("Installing nerdctl, CNI plugins and BuildKit in the VM...");
            call(paths, "enable nerdctl")?;
            install_nerdctl_wrapper(paths)?;
            println!("nerdctl is ready: `nerdctl run ...` (runs in the VM, on containerd's 'default' namespace)");
        }
        _ => bail!("unknown engine {name}: use podman or nerdctl"),
    }
    Ok(())
}

pub fn disable(paths: &Paths, name: &str) -> Result<()> {
    if !matches!(name, "podman" | "nerdctl") {
        bail!("unknown engine {name}: use podman or nerdctl");
    }
    call(paths, &format!("disable {name}"))?;
    println!("{name} stopped (its containers and images are kept)");
    Ok(())
}

pub fn status(paths: &Paths) -> Result<()> {
    let status: serde_json::Value = serde_json::from_str(&call(paths, "status")?)?;
    let on = |key: &str| if status[key].as_bool() == Some(true) { "enabled" } else { "off (berth engine enable <name>)" };
    println!("docker   enabled");
    println!("podman   {}", on("podman"));
    println!("nerdctl  {}", on("nerdctl"));
    Ok(())
}

fn install_podman_remote(paths: &Paths) -> Result<()> {
    let bin = crate::cli_tools::bin_dir(paths);
    let podman = bin.join("podman");
    let stamp = bin.join(".podman-version");
    if !podman.exists() || fs::read_to_string(&stamp).ok().as_deref() != Some(PODMAN_REMOTE) {
        let zip = bin.join("podman-remote.zip");
        crate::cli_tools::fetch_verified_sha256(
            &zip,
            &format!("https://github.com/containers/podman/releases/download/{PODMAN_REMOTE}/podman-remote-release-darwin_arm64.zip"),
            PODMAN_REMOTE_SHA256,
        )?;
        let inner = format!("podman-{}/usr/bin/podman", PODMAN_REMOTE.trim_start_matches('v'));
        let status = Command::new("unzip").args(["-o", "-j", "-q"]).arg(&zip).arg(&inner).arg("-d").arg(&bin).status()?;
        fs::remove_file(&zip).ok();
        if !status.success() {
            bail!("extracting podman failed");
        }
        fs::write(&stamp, PODMAN_REMOTE)?;
    }
    // Make berth's socket the default connection (replacing an older entry).
    let url = format!("unix://{}", paths.podman_socket().display());
    let _ = Command::new(&podman).args(["system", "connection", "rm", "berth"]).output();
    let out = Command::new(&podman)
        .args(["system", "connection", "add", "--default", "berth", &url])
        .output()
        .context("running podman")?;
    if !out.status.success() {
        bail!("podman system connection add: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn install_nerdctl_wrapper(paths: &Paths) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let bin = crate::cli_tools::bin_dir(paths);
    // The stable path when berth is installed; else this binary.
    let installed = std::path::Path::new("/usr/local/bin/berth");
    let berth = if installed.exists() { installed.to_path_buf() } else { std::env::current_exe()? };
    let wrapper = bin.join("nerdctl");
    fs::write(&wrapper, format!("#!/bin/sh\n# nerdctl runs in the berth VM.\nexec '{}' nerdctl \"$@\"\n", berth.display()))?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// The command `berth nerdctl` runs in the VM: nerdctl as root, from the
/// caller's directory when it exists there (anything under home does).
pub fn nerdctl_command(args: &[String]) -> Vec<String> {
    let quote = |w: &str| format!("'{}'", w.replace('\'', "'\\''"));
    let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let inner = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    let script = format!("cd {} 2>/dev/null; exec nerdctl {inner}", quote(&cwd));
    vec!["sudo".into(), "sh".into(), "-c".into(), quote(&script)]
}
