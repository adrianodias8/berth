//! What `berth gui session new --clone` needs from macOS VMs, as a thin
//! adapter over the `berth mac` commands (src/macvm.rs) run as a child
//! process of this same binary. Going through the CLI keeps this module
//! independent of macvm.rs's Rust API, and the VM work (APFS clones, its
//! own process per VM) is the mac commands' business anyway.
//!
//! Relies on:
//!   berth mac clone <base> <name>       an APFS copy of a stopped base VM
//!   berth mac start <name>              returns once the VM's process runs
//!   berth mac stop <name>
//!   berth mac delete <name> --yes
//!   berth mac list --json               [{"name", "state", "base", …}]
//! and on the files of docs/gui-protocol.md: a running VM has
//! ~/.berth/mac/<name>/run.pid.

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::paths::Paths;

/// Apple's license: at most two macOS VMs at once on a Mac.
pub const MAX_RUNNING: usize = 2;

#[derive(Deserialize, Debug)]
pub struct Vm {
    pub name: String,
}

fn berth(args: &[&str]) -> Result<String> {
    let exe = std::env::current_exe()?;
    let out = Command::new(exe).args(args).output().with_context(|| format!("running berth {}", args.join(" ")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("berth {} failed: {}", args.join(" "), err.trim().trim_start_matches("Error: "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn list() -> Result<Vec<Vm>> {
    let text = berth(&["mac", "list", "--json"])?;
    serde_json::from_str(&text).context("berth mac list --json gave something else than a list")
}

pub fn clone(base: &str, name: &str) -> Result<()> {
    berth(&["mac", "clone", base, name]).map(drop)
}

pub fn start(name: &str) -> Result<()> {
    berth(&["mac", "start", name]).map(drop)
}

/// Stop (if running) and delete a session's clone.
pub fn delete(name: &str) -> Result<()> {
    let _ = berth(&["mac", "stop", name]);
    berth(&["mac", "delete", name, "--yes"]).map(drop)
}

/// macOS VMs running now, by their run.pid files: cheaper than `mac list`,
/// and true even while another `berth gui` is starting one.
pub fn running(paths: &Paths) -> usize {
    let Ok(dirs) = std::fs::read_dir(paths.root.join("mac")) else { return 0 };
    dirs.flatten()
        .filter_map(|d| std::fs::read_to_string(d.path().join("run.pid")).ok())
        .filter_map(|pid| pid.trim().parse::<u32>().ok())
        .filter(|&pid| super::sessions::alive(pid))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_vms_are_counted_by_live_pid_files() {
        let root = std::env::temp_dir().join(format!("berth-macvms-{}", crate::gui_log::new_id()));
        let paths = Paths { home: root.clone(), root: root.clone() };
        assert_eq!(running(&paths), 0);
        for (name, pid) in [("a", std::process::id().to_string()), ("b", "999999".to_string()), ("c", "junk".to_string())] {
            std::fs::create_dir_all(root.join("mac").join(name)).unwrap();
            std::fs::write(root.join("mac").join(name).join("run.pid"), pid).unwrap();
        }
        std::fs::create_dir_all(root.join("mac/stopped")).unwrap();
        assert_eq!(running(&paths), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
