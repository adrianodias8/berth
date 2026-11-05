//! First-start provisioning: base image download, data disk, SSH key and the
//! cloud-init seed ISO.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

const IMAGE_BASE: &str = "https://cloud.debian.org/images/cloud/trixie/latest";
const IMAGE_NAME: &str = "debian-13-generic-arm64.tar.xz";
/// A new VM disk's size, unless `disk_gib` in config.json says otherwise.
const DISK_SIZE: u64 = 64 << 30;
const GIB: u64 = 1 << 30;

const USER_DATA_TEMPLATE: &str = include_str!("../guest/user-data.yaml");
const AGENT: &[u8] = include_bytes!(env!("BERTH_AGENT"));

/// Identifies the embedded agent build; the agent hashes its own binary the
/// same way (`info version`).
pub fn agent_version() -> String {
    let h = AGENT.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

/// Make sure everything the VM needs exists. Cheap when already provisioned.
pub fn ensure(paths: &Paths) -> Result<()> {
    paths.create_dirs()?;
    exclude_from_backups(paths);
    if !paths.disk().exists() {
        let raw = fetch_base_image(paths)?;
        create_disk(&raw, &paths.disk(), configured_disk_size(paths).unwrap_or(DISK_SIZE))?;
    }
    if !paths.ssh_key().exists() {
        run(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "berth", "-f"])
            .arg(paths.ssh_key()))?;
    }
    if !paths.seed_iso().exists() {
        write_seed_iso(paths)?;
    }
    install_agent(paths)
}

/// Put the current guest agent where the guest's systemd unit runs it from.
/// Written to a temp file and renamed, so a running guest never sees a
/// half-written binary.
fn install_agent(paths: &Paths) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dest = paths.agent();
    if fs::read(&dest).is_ok_and(|current| current == AGENT) {
        return Ok(());
    }
    let tmp = dest.with_extension("tmp");
    fs::write(&tmp, AGENT)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, &dest)?;
    Ok(())
}

/// Keep the VM disk and image cache out of Time Machine: a large sparse disk
/// image is rewritten constantly, and backing it up bloats (or, on HFS+
/// destinations, breaks) backups. A "sticky" exclusion needs no root and
/// follows the folder.
fn exclude_from_backups(paths: &Paths) {
    for dir in [paths.data(), paths.cache()] {
        let excluded = Command::new("tmutil")
            .arg("isexcluded")
            .arg(&dir)
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("[Excluded]"));
        if !excluded {
            let _ = Command::new("tmutil").arg("addexclusion").arg(&dir).output();
        }
    }
}

/// Download, verify and unpack the Debian cloud image. Returns the raw disk path.
fn fetch_base_image(paths: &Paths) -> Result<std::path::PathBuf> {
    let cache = paths.cache();
    let raw = cache.join("debian-13-generic-arm64.raw");
    if raw.exists() {
        return Ok(raw);
    }

    let archive = cache.join(IMAGE_NAME);
    if !archive.exists() {
        eprintln!("Downloading Debian 13 base image (one time, ~350 MB)...");
        let part = cache.join(format!("{IMAGE_NAME}.part"));
        run(Command::new("curl")
            .args(["--fail", "--location", "--progress-bar", "--continue-at", "-", "-o"])
            .arg(&part)
            .arg(format!("{IMAGE_BASE}/{IMAGE_NAME}")))?;

        let sums = output(Command::new("curl").args([
            "--fail",
            "--silent",
            "--location",
            &format!("{IMAGE_BASE}/SHA512SUMS"),
        ]))?;
        let expected = sums
            .lines()
            .find_map(|line| line.strip_suffix(&format!("  {IMAGE_NAME}")))
            .context("image checksum not found in SHA512SUMS")?
            .to_string();
        let actual = output(Command::new("shasum").args(["-a", "512"]).arg(&part))?;
        if actual.split_whitespace().next() != Some(expected.as_str()) {
            fs::remove_file(&part).ok();
            bail!("checksum mismatch for {IMAGE_NAME}; the partial download was removed, try again");
        }
        fs::rename(&part, &archive)?;
    }

    eprintln!("Unpacking base image...");
    let unpack = cache.join("unpack");
    fs::create_dir_all(&unpack)?;
    run(Command::new("tar").arg("-xJf").arg(&archive).arg("-C").arg(&unpack))?;
    fs::rename(unpack.join("disk.raw"), &raw).context("disk.raw missing from image archive")?;
    fs::remove_dir_all(&unpack).ok();
    Ok(raw)
}

/// Make the VM disk from the base image. On macOS 26+ it is an ASIF image:
/// Apple's sparse format that gives space back when the guest discards
/// blocks, so deleting images and containers frees disk space on the Mac (a
/// raw sparse file mostly doesn't). Elsewhere a raw copy, cloned instantly by
/// APFS. Either way it grows to `size`; cloud-init grows the partition.
fn create_disk(base: &Path, disk: &Path, size: u64) -> Result<()> {
    if asif_supported() {
        let tmp = disk.with_extension("asif");
        let _ = fs::remove_file(&tmp);
        run(Command::new("diskutil").args(["image", "create", "from", "--format", "ASIF"]).arg(base).arg(&tmp).stdout(std::process::Stdio::null()))?;
        resize(&tmp, size)?;
        fs::rename(&tmp, disk)?;
        return Ok(());
    }
    let tmp = disk.with_extension("tmp");
    fs::copy(base, &tmp).context("copying base image")?;
    resize(&tmp, size)?;
    fs::rename(&tmp, disk)?;
    Ok(())
}

/// `disk_gib` in config.json, in bytes.
pub fn configured_disk_size(paths: &Paths) -> Option<u64> {
    let config: serde_json::Value = serde_json::from_slice(&fs::read(paths.config()).ok()?).ok()?;
    Some(config.get("disk_gib")?.as_u64()?.saturating_mul(GIB))
}

/// ASIF images start with "shdw"; anything else is a raw disk.
fn is_asif(disk: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    fs::File::open(disk).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == b"shdw"
}

/// The disk's size as the guest sees it. For an ASIF image diskutil reads it,
/// which it can't while the VM has the image open.
pub fn disk_size(disk: &Path) -> Option<u64> {
    if !is_asif(disk) {
        return fs::metadata(disk).ok().map(|m| m.len());
    }
    let plist = output(Command::new("diskutil").args(["image", "info", "--plist"]).arg(disk)).ok()?;
    plist_integer(&plist, "Total Bytes")
}

/// The integer after `<key>{key}</key>` in an XML plist.
fn plist_integer(plist: &str, key: &str) -> Option<u64> {
    let rest = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = rest.find("<integer>")? + "<integer>".len();
    rest[start..start + rest[start..].find('<')?].trim().parse().ok()
}

/// Set the disk's size: the image only; the guest grows its partition and
/// filesystem when it boots (agent/src/disk.rs).
fn resize(disk: &Path, size: u64) -> Result<()> {
    if is_asif(disk) {
        run(Command::new("diskutil")
            .args(["image", "resize", "--image-only", "--size", &size.to_string()])
            .arg(disk)
            .stdout(std::process::Stdio::null()))
    } else {
        // Raw: the guest's growpart moves GPT's backup table to the new end.
        fs::OpenOptions::new().write(true).open(disk)?.set_len(size)?;
        Ok(())
    }
}

/// How the disk would change for `disk_gib`: Some((now, wanted)) when it
/// should grow. Disks only grow: ext4 can't shrink while mounted, and a
/// smaller image would cut off data.
pub fn disk_growth(paths: &Paths) -> Option<(u64, u64)> {
    let want = configured_disk_size(paths)?;
    let have = disk_size(&paths.disk())?;
    (want > whole_gib(have)).then_some((have, want))
}

/// A size rounded up to whole GiB, as `disk_gib` and the app count it: a
/// disk made as "64G" (64e9 bytes, 59.6 GiB) is 60, and `disk_gib` 60
/// leaves it as it is.
pub fn whole_gib(bytes: u64) -> u64 {
    bytes.div_ceil(GIB) * GIB
}

/// Grow the VM disk to `disk_gib`. Only while the VM is off and booting
/// (not resuming a saved state, whose disk size is fixed in it).
pub fn grow_disk(paths: &Paths) -> Result<()> {
    let Some((have, want)) = disk_growth(paths) else {
        if let (Some(want), Some(have)) = (configured_disk_size(paths), disk_size(&paths.disk()))
            && want < whole_gib(have)
        {
            eprintln!(
                "disk: disk_gib is {} but the disk is already {:.1} GiB; disks only grow, so it stays",
                want / GIB,
                have as f64 / GIB as f64
            );
        }
        return Ok(());
    };
    eprintln!("disk: growing the VM disk from {:.1} to {:.1} GiB", have as f64 / GIB as f64, want as f64 / GIB as f64);
    resize(&paths.disk(), want)
}

/// ASIF images (and `diskutil image`) arrived in macOS 26.
fn asif_supported() -> bool {
    output(Command::new("sw_vers").arg("-productVersion"))
        .ok()
        .and_then(|v| v.trim().split('.').next()?.parse::<u32>().ok())
        .is_some_and(|major| major >= 26)
}

fn write_seed_iso(paths: &Paths) -> Result<()> {
    let user = std::env::var("USER").context("USER is not set")?;
    let uid = unsafe { libc::getuid() };
    let ssh_key = fs::read_to_string(paths.ssh_key().with_extension("pub"))?;
    let user_data = USER_DATA_TEMPLATE
        .replace("@USER@", &user)
        .replace("@UID@", &uid.to_string())
        .replace("@HOME@", &paths.home.to_string_lossy())
        .replace("@SSH_KEY@", ssh_key.trim());

    let dir = paths.data().join("seed");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("user-data"), user_data)?;
    fs::write(dir.join("meta-data"), "instance-id: berth-1\nlocal-hostname: berth\n")?;

    let iso = paths.seed_iso();
    run(Command::new("hdiutil")
        .args(["makehybrid", "-quiet", "-iso", "-joliet", "-default-volume-name", "cidata", "-o"])
        .arg(&iso)
        .arg(&dir))?;
    fs::remove_dir_all(&dir).ok();
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed with {status}");
    }
    Ok(())
}

fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plist_integers() {
        let plist = "<dict>\n\t<key>Size Info</key>\n\t<dict>\n\t\t<key>Empty Bytes</key>\n\t\t<integer>8457654272</integer>\n\t\t<key>Total Bytes</key>\n\t\t<integer>64000000000</integer>\n\t</dict>";
        assert_eq!(plist_integer(plist, "Total Bytes"), Some(64_000_000_000));
        assert_eq!(plist_integer(plist, "Empty Bytes"), Some(8_457_654_272));
        assert_eq!(plist_integer(plist, "Sector Count"), None);
    }

    #[test]
    fn whole_gib_rounds_up() {
        assert_eq!(whole_gib(64_000_000_000), 60 * GIB);
        assert_eq!(whole_gib(128 * GIB), 128 * GIB);
        assert_eq!(whole_gib(128 * GIB + 1), 129 * GIB);
    }
}
