//! berth's polling virtio-fs driver (guest/kmod/virtiofs_poll), built here
//! for whatever kernel is running, and the mount helper that uses it.
//!
//! The host presents its file sharing device under berth's own device ID,
//! which only this module binds, once `~/.berth/guest/kmod-ready` names the
//! kernel it's about to boot; until then it uses the virtio-fs ID and the
//! built-in driver serves it. Either way the home share is mounted by
//! `mount.berth`, which tries the polling driver first.
//!
//! Building needs the kernel's headers and a compiler (fetched with apt the
//! first time, and again after a kernel update); failures are retried later
//! and only cost speed.
//!
//! If the VM ever fails to come up, the host boots it in safe mode
//! (`berth.safe=1`, home mounted without /etc/fstab, see vm.rs). Then the
//! home line goes back to plain virtiofs and this helper stays unused until
//! a berth with a different one arrives.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const SOURCE_C: &str = include_str!("../../guest/kmod/virtiofs_poll/virtiofs_poll.c");
const SOURCE_H: &str = include_str!("../../guest/kmod/virtiofs_poll/fuse_i.h");
const BUILD_DIR: &str = "/var/lib/berth/kmod";
/// Holds the hash of a helper that was in use when the VM didn't come up.
const HELPER_FAILED: &str = "/var/lib/berth/mount-helper-failed";

const HELPER_PATH: &str = "/usr/sbin/mount.berth";
const HELPER: &str = r#"#!/bin/sh
# Installed by berth-agent (agent/src/kmod.rs): /etc/fstab mounts the home
# share as type "berth". berth's polling driver serves it when the host
# presents its device for it; otherwise the built-in virtio-fs driver does.
# mount(8) hands helpers "/home" for the tag "home" (a source that looks
# like an existing path is canonicalized): the tag is the last component.
spec=${1##*/} dir=$2
shift 2
modprobe -q virtiofs_poll 2>/dev/null
# --no-canonicalize: mount(8) would rewrite the tag again for virtiofs_poll.
mount -i --no-canonicalize -t virtiofs_poll "$spec" "$dir" "$@" 2>/dev/null ||
  exec mount -i --no-canonicalize -t virtiofs "$spec" "$dir" "$@"
"#;

fn release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string()
}

fn fnv(bytes: impl Iterator<Item = u8>) -> String {
    format!("{:016x}", bytes.fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3)))
}

/// Changes whenever the module's source does, so a new berth rebuilds it.
fn source_hash() -> String {
    fnv(SOURCE_C.bytes().chain(SOURCE_H.bytes()))
}

fn safe_boot() -> bool {
    std::fs::read_to_string("/proc/cmdline").is_ok_and(|c| c.split_whitespace().any(|a| a == "berth.safe=1"))
}

/// Point the fstab home line at `from` instead of `to`.
fn set_home_type(from: &str, to: &str) -> io::Result<bool> {
    let fstab = std::fs::read_to_string("/etc/fstab")?;
    let updated: Vec<String> = fstab
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 3 && fields[0] == "home" && fields[2] == from {
                fields[2] = to;
                fields.join("\t")
            } else {
                line.to_string()
            }
        })
        .collect();
    let updated = updated.join("\n") + "\n";
    if updated == fstab {
        return Ok(false);
    }
    // Durable before the rename: a torn fstab is a VM that doesn't boot.
    let mut tmp = std::fs::File::create("/etc/fstab.berth-tmp")?;
    io::Write::write_all(&mut tmp, updated.as_bytes())?;
    tmp.sync_all()?;
    std::fs::rename("/etc/fstab.berth-tmp", "/etc/fstab")?;
    Ok(true)
}

fn module_dir(release: &str) -> PathBuf {
    PathBuf::from(format!("/lib/modules/{release}/updates/berth"))
}

/// `~/.berth/guest/kmod-ready`, next to the agent binary.
/// Running berth's own kernel (guest/kernel), which has the polling driver
/// built in.
pub fn berth_kernel() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|r| r.trim().ends_with("-berth"))
}

/// berth's kernel has everything built in; modprobe still wants its
/// /lib/modules directory (modules.builtin), which the host stages as
/// ~/.berth/guest/kernel-modules.tar (vm.rs).
fn install_berth_modules() -> io::Result<()> {
    let Some(tar) = marker().map(|m| m.with_file_name("kernel-modules.tar")) else { return Ok(()) };
    let release = release();
    let stamp = format!("/lib/modules/{release}/.berth-modules");
    // The host copies the tar only when it changed, so size and mtime tell
    // (a same-size rebuild, in 512-byte tar blocks, is likely).
    let meta = std::fs::metadata(&tar)?;
    let modified = meta.modified()?.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let size = format!("{} {modified}", meta.len());
    if std::fs::read_to_string(&stamp).is_ok_and(|s| s == size) {
        return Ok(());
    }
    sh(Command::new("tar").arg("-xf").arg(&tar).args(["-C", "/"]))?;
    sh(Command::new("depmod").arg(&release))?;
    std::fs::write(stamp, size)?;
    eprintln!("berth-agent: kmod: installed module data for {release}");
    Ok(())
}

fn marker() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("kmod-ready"))
}

/// Why the polling driver isn't there (`~/.berth/guest/kmod-error`), for
/// `berth status` and the app; removed once it is.
fn report(error: Option<&str>) {
    let Some(path) = marker().map(|m| m.with_file_name("kmod-error")) else { return };
    match error {
        Some(error) => {
            let _ = std::fs::write(path, format!("{error}\n"));
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn sh(cmd: &mut Command) -> io::Result<()> {
    let out = cmd.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("failed").to_string()));
    }
    Ok(())
}

fn install_mount_helper() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::read_to_string(HELPER_PATH).ok().as_deref() != Some(HELPER) {
        let tmp = format!("{HELPER_PATH}.tmp");
        std::fs::write(&tmp, HELPER)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, HELPER_PATH)?;
    }
    // The home line (from cloud-init) names virtiofs; mount it through the helper.
    if set_home_type("virtiofs", "berth")? {
        eprintln!("berth-agent: kmod: home share now mounts through {HELPER_PATH}");
    }
    Ok(())
}

fn build(release: &str) -> io::Result<()> {
    let dir = module_dir(release);
    let hash = source_hash();
    if dir.join("virtiofs_poll.ko").exists() && std::fs::read_to_string(dir.join("source")).ok().as_deref() == Some(&hash) {
        return Ok(());
    }
    let headers = format!("/lib/modules/{release}/build");
    if !Path::new(&headers).exists() {
        eprintln!("berth-agent: kmod: installing headers for {release} and a compiler");
        let install = || {
            sh(Command::new("apt-get")
                .args(["install", "-y", "-q", &format!("linux-headers-{release}"), "gcc", "make"])
                .env("DEBIAN_FRONTEND", "noninteractive"))
        };
        if install().is_err() {
            sh(Command::new("apt-get").args(["update", "-q"]))?;
            install()?;
        }
    }
    let _ = std::fs::remove_dir_all(BUILD_DIR);
    std::fs::create_dir_all(BUILD_DIR)?;
    std::fs::write(format!("{BUILD_DIR}/virtiofs_poll.c"), SOURCE_C)?;
    std::fs::write(format!("{BUILD_DIR}/fuse_i.h"), SOURCE_H)?;
    std::fs::write(format!("{BUILD_DIR}/Kbuild"), "obj-m := virtiofs_poll.o\n")?;
    sh(Command::new("make").args(["-C", &headers, &format!("M={BUILD_DIR}"), "modules"]))?;
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(format!("{BUILD_DIR}/virtiofs_poll.ko"), dir.join("virtiofs_poll.ko"))?;
    std::fs::write(dir.join("source"), &hash)?;
    sh(Command::new("depmod").arg(release))?;
    eprintln!("berth-agent: kmod: built the polling driver for {release}");
    Ok(())
}

pub fn run() {
    let helper_hash = fnv(HELPER.bytes());
    if safe_boot() {
        // The last normal boot didn't come up: back to what cloud-init set up,
        // and, if the helper was in use, no polling driver (it needs the
        // helper) until berth changes it. Something else may have kept the
        // boot from coming up; then the helper isn't to blame.
        match set_home_type("berth", "virtiofs") {
            Ok(true) => {
                let _ = std::fs::create_dir_all("/var/lib/berth");
                let _ = std::fs::write(HELPER_FAILED, &helper_hash);
                eprintln!("berth-agent: kmod: safe boot: home mounts as plain virtiofs again");
            }
            Ok(false) => eprintln!("berth-agent: kmod: safe boot: the mount helper wasn't in use"),
            Err(err) => eprintln!("berth-agent: kmod: safe boot: restoring /etc/fstab: {err}"),
        }
        if let Some(marker) = marker() {
            let _ = std::fs::remove_file(marker);
        }
        return;
    }
    if std::fs::read_to_string(HELPER_FAILED).ok().as_deref() == Some(helper_hash.as_str()) {
        report(Some("the VM didn't come up with it last time; off until berth is updated"));
        return;
    }
    if let Err(err) = install_mount_helper() {
        eprintln!("berth-agent: kmod: mount helper: {err}");
        report(Some(&format!("mount helper: {err}")));
        return;
    }
    if berth_kernel() {
        if let Err(err) = install_berth_modules() {
            eprintln!("berth-agent: kmod: module data: {err}");
        }
        report(None);
        return;
    }
    let release = release();
    for attempt in 0.. {
        match build(&release) {
            Ok(()) => {
                if let Some(marker) = marker() {
                    let _ = std::fs::write(marker, format!("{release}\n"));
                }
                report(None);
                return;
            }
            Err(err) => {
                eprintln!("berth-agent: kmod: building the polling driver: {err}");
                report(Some(&format!("building it failed ({err}); retrying")));
                // Offline, most likely: try again later.
                std::thread::sleep(Duration::from_secs(if attempt < 3 { 60 } else { 1800 }));
            }
        }
    }
}
