//! Boot-time tuning: mostly applied once and effective from the next boot;
//! the disk scheduler is set on every boot.
//!
//! Measured on Debian 13 (systemd-analyze): Docker's start was gated behind
//! systemd-networkd-wait-online (~1.2 s waiting for DHCP) via cloud-init's
//! network stage, and cloud-init itself ran on every boot though it only
//! provisions on the first.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

const CLOUD_INIT_DONE: &str = "/var/lib/cloud/instance/boot-finished";
const CLOUD_INIT_DISABLED: &str = "/etc/cloud/cloud-init.disabled";

/// /boot/efi is only needed when GRUB is updated, yet mounting it held up
/// local-fs.target (and so Docker) for ~0.9 s. Mount it on first access.
fn lazy_efi_mount() {
    let Ok(fstab) = std::fs::read_to_string("/etc/fstab") else { return };
    if fstab.contains("x-systemd.automount") {
        return;
    }
    let updated: String = fstab
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 4 && fields[1] == "/boot/efi" {
                let mut fields = fields.clone();
                let options = format!("{},noauto,x-systemd.automount", fields[3]);
                fields[3] = &options;
                fields.join(" ")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    if updated != fstab && std::fs::write("/etc/fstab.berth-tmp", &updated).is_ok() && std::fs::rename("/etc/fstab.berth-tmp", "/etc/fstab").is_ok() {
        eprintln!("berth-agent: boot: /boot/efi now mounts on demand");
    }
}

/// Export the running kernel, initrd and command line to the shared
/// ~/.berth/guest/boot so the host can boot it directly (no EFI firmware or
/// GRUB, see vm.rs). Copies only when the kernel changed (e.g. after an apt
/// upgrade). Also clears the host's "trying" marker: reaching this point means
/// a direct boot worked.
pub fn export_kernel() {
    let Some(dir) = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("boot"))) else { return };
    // A safe boot's command line is the host's one-off (vm.rs): keep the
    // exported one.
    if std::fs::read_to_string("/proc/cmdline").is_ok_and(|c| c.contains("berth.safe=1")) {
        let _ = std::fs::remove_file(dir.join("trying"));
        let _ = std::fs::remove_file(dir.join("kmod-trying"));
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    // berth's own kernel (vm.rs) came up: clear its marker, and keep the
    // Debian kernel's export as it is, for when it's chosen or needed.
    if crate::kmod::berth_kernel() {
        let _ = std::fs::remove_file(dir.join("berth-trying"));
        let _ = std::fs::remove_file(dir.join("kmod-trying"));
        return;
    }
    let _ = std::fs::remove_file(dir.join("trying"));
    // Same for the host's polling-driver attempt (vm.rs): we're up, so the
    // home share mounted.
    let _ = std::fs::remove_file(dir.join("kmod-trying"));

    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let release = release.trim();
    let mut changed = false;
    for (src, dest) in [(format!("/boot/vmlinuz-{release}"), "vmlinuz"), (format!("/boot/initrd.img-{release}"), "initrd.img")] {
        let dest = dir.join(dest);
        let same = matches!(
            (std::fs::metadata(&src), std::fs::metadata(&dest)),
            (Ok(a), Ok(b)) if a.len() == b.len() && std::fs::read(&src).ok() == std::fs::read(&dest).ok()
        );
        if same {
            continue;
        }
        let tmp = dest.with_extension("tmp");
        if std::fs::copy(&src, &tmp).is_ok() && std::fs::rename(&tmp, &dest).is_ok() {
            changed = true;
        } else {
            eprintln!("berth-agent: boot: could not export {src}");
            return;
        }
    }
    // The command line GRUB used, minus its own BOOT_IMAGE, with the console
    // on the virtio port the host logs.
    let cmdline: Vec<String> = std::fs::read_to_string("/proc/cmdline")
        .unwrap_or_default()
        .split_whitespace()
        .filter(|t| !t.starts_with("BOOT_IMAGE=") && !t.starts_with("console="))
        .map(str::to_string)
        .chain(["console=hvc0".to_string()])
        .collect();
    let _ = std::fs::write(dir.join("cmdline"), cmdline.join(" "));
    let _ = std::fs::write(dir.join("release"), release);
    if changed {
        // A new kernel gets a fresh chance at direct boot.
        let _ = std::fs::remove_file(dir.join("direct-failed"));
        eprintln!("berth-agent: boot: exported kernel {release} for direct boot");
    }
}

pub fn tune() {
    lazy_efi_mount();
    // The host already orders and merges the VM's disk I/O; a second
    // scheduler in the guest only adds latency (and timers).
    for disk in std::fs::read_dir("/sys/block").into_iter().flatten().flatten() {
        if disk.file_name().to_string_lossy().starts_with("vd") {
            let _ = std::fs::write(disk.path().join("queue/scheduler"), "none");
        }
    }
    // Docker doesn't need DHCP to have finished; container networking comes
    // up a moment later on its own.
    let _ = Command::new("systemctl").args(["mask", "--quiet", "systemd-networkd-wait-online.service"]).status();

    // On a fresh VM we start while cloud-init is still provisioning: wait for
    // it to finish before turning it off for later boots.
    if Path::new(CLOUD_INIT_DISABLED).exists() {
        return;
    }
    for _ in 0..600 {
        if Path::new(CLOUD_INIT_DONE).exists() {
            if std::fs::write(CLOUD_INIT_DISABLED, "disabled by berth after first-boot provisioning\n").is_ok() {
                eprintln!("berth-agent: boot: cloud-init disabled for later boots");
            }
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
