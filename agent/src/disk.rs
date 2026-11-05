//! The VM's root disk: grow it into space the host added (`disk_gib`,
//! src/provision.rs), and report how full it is (`info disk`).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(serde::Serialize)]
pub struct Usage {
    /// Bytes of the root filesystem, and how many are free for use.
    pub total: u64,
    pub available: u64,
}

pub fn usage() -> std::io::Result<Usage> {
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c"/".as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let block = st.f_frsize as u64;
    Ok(Usage { total: st.f_blocks as u64 * block, available: st.f_bavail as u64 * block })
}

/// The root filesystem's partition in sysfs (…/block/vda/vda2), from the
/// device number of `/`.
fn root_partition() -> Option<PathBuf> {
    let dev = std::fs::metadata("/").ok()?.dev();
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()
}

fn number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The ext4 filesystem's size from its superblock (1024 bytes into the
/// partition): what resize2fs grows, unlike statvfs, which leaves out
/// metadata.
fn ext4_size(part: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut sb = [0u8; 1024];
    let mut dev = std::fs::File::open(part).ok()?;
    dev.seek(SeekFrom::Start(1024)).ok()?;
    dev.read_exact(&mut sb).ok()?;
    let u32_at = |o: usize| u32::from_le_bytes([sb[o], sb[o + 1], sb[o + 2], sb[o + 3]]) as u64;
    if u16::from_le_bytes([sb[0x38], sb[0x39]]) != 0xEF53 {
        return None;
    }
    // The high half of the block count only with the 64bit feature.
    let hi = if u32_at(0x60) & 0x80 != 0 { u32_at(0x150) } else { 0 };
    let blocks = (hi << 32) | u32_at(0x04);
    Some(blocks << (10 + u32_at(0x18)))
}

fn run(what: &str, cmd: &mut Command) -> bool {
    match cmd.output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            let text = [out.stdout, out.stderr].map(|b| String::from_utf8_lossy(&b).trim().to_string()).join(" ");
            eprintln!("berth-agent: disk: {what}: {}", text.trim());
            false
        }
        Err(err) => {
            eprintln!("berth-agent: disk: {what}: {err}");
            false
        }
    }
}

/// Space smaller than this (64 MiB) isn't worth growing into; GPT also
/// keeps a backup table in the disk's last sectors.
const SLACK: u64 = 64 << 20;

/// The host grows the disk image while the VM is off; here the partition
/// and then ext4 (online) follow. Each step checks on its own, so one that
/// was interrupted (the VM stopped mid-resize) is finished on the next
/// boot. Cheap when there's nothing to do: a few sysfs reads and the
/// superblock. On the first boot cloud-init grows the partition itself.
pub fn grow_root() {
    if !Path::new(crate::boot::CLOUD_INIT_DISABLED).exists() {
        return;
    }
    let Some(part) = root_partition() else { return };
    let Some(disk) = part.parent() else { return };
    let (Some(disk_name), Some(part_name)) = (disk.file_name(), part.file_name()) else { return };
    let (disk_dev, part_dev) = (Path::new("/dev").join(disk_name), Path::new("/dev").join(part_name));
    let (Some(disk_sectors), Some(start), Some(number_)) =
        (number(&disk.join("size")), number(&part.join("start")), number(&part.join("partition")))
    else {
        return;
    };
    let Some(size) = number(&part.join("size")) else { return };
    if (disk_sectors.saturating_sub(start + size)) * 512 >= SLACK
        && !run("growpart", Command::new("growpart").arg(&disk_dev).arg(number_.to_string()))
    {
        return;
    }
    // growpart has the kernel re-read the table: the partition's new size.
    let Some(part_bytes) = number(&part.join("size")).map(|s| s * 512) else { return };
    let Some(fs_bytes) = ext4_size(&part_dev) else { return };
    if part_bytes.saturating_sub(fs_bytes) < SLACK {
        return;
    }
    if run("resize2fs", Command::new("resize2fs").arg(&part_dev)) {
        let gib = usage().map(|u| u.total as f64 / (1u64 << 30) as f64).unwrap_or_default();
        eprintln!("berth-agent: disk: grew {} to {gib:.1} GiB", part_dev.display());
    }
}
