//! Boot-time tuning, applied once and effective from the next boot.
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

pub fn tune() {
    lazy_efi_mount();
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
