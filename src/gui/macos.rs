//! What `berth gui session new --clone` needs from macOS VMs, as a thin
//! adapter over the `berth mac` commands (src/macvm.rs) run as a child
//! process of this same binary. Going through the CLI keeps this module
//! independent of macvm.rs's Rust API, and the VM work (APFS clones, its
//! own process per VM) is the mac commands' business anyway.
//!
//! Relies on:
//!   berth mac clone <base> <name>       an APFS copy of a stopped base VM
//!   berth mac start <name>              returns once the VM's process runs
//!   berth mac stop <name> --force
//!   berth mac delete <name> --yes
//!   berth mac list --json               [{"name", "state", "base", …}]
//! and, in-process, on `macvm::running` (VMs whose run.pid lock is held)
//! and `macvm::default_base`.

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::paths::Paths;

/// Apple's license: at most two macOS VMs at once on a Mac.
pub use crate::macvm::MAX_RUNNING;

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

/// Stop (if running) and delete a session's clone. It's thrown away, so
/// no guest shutdown first.
pub fn delete(name: &str) -> Result<()> {
    let _ = berth(&["mac", "stop", name, "--force"]);
    berth(&["mac", "delete", name, "--yes"]).map(drop)
}

/// macOS VMs running now: cheaper than `mac list`, and true even while
/// another `berth gui` is starting one (its process holds run.pid's lock).
pub fn running(paths: &Paths) -> usize {
    crate::macvm::running(paths).len()
}

/// The VM `berth mac set-base` saved last.
pub fn default_base(paths: &Paths) -> Option<String> {
    crate::macvm::default_base(paths)
}
