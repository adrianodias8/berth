//! First-start provisioning: base image download, data disk, SSH key and the
//! cloud-init seed ISO.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

const IMAGE_BASE: &str = "https://cloud.debian.org/images/cloud/trixie/latest";
const IMAGE_NAME: &str = "debian-13-generic-arm64.tar.xz";
const DISK_SIZE: u64 = 64 << 30;

const USER_DATA_TEMPLATE: &str = include_str!("../guest/user-data.yaml");
const AGENT: &[u8] = include_bytes!(env!("BERTH_AGENT"));

/// Make sure everything the VM needs exists. Cheap when already provisioned.
pub fn ensure(paths: &Paths) -> Result<()> {
    paths.create_dirs()?;
    exclude_from_backups(paths);
    if !paths.disk().exists() {
        let raw = fetch_base_image(paths)?;
        create_disk(&raw, &paths.disk())?;
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
/// APFS. Either way it grows to DISK_SIZE; cloud-init grows the partition.
fn create_disk(base: &Path, disk: &Path) -> Result<()> {
    if asif_supported() {
        let tmp = disk.with_extension("asif");
        let _ = fs::remove_file(&tmp);
        run(Command::new("diskutil").args(["image", "create", "from", "--format", "ASIF"]).arg(base).arg(&tmp).stdout(std::process::Stdio::null()))?;
        run(Command::new("diskutil")
            .args(["image", "resize", "--image-only", "--size", &format!("{}G", DISK_SIZE >> 30)])
            .arg(&tmp)
            .stdout(std::process::Stdio::null()))?;
        fs::rename(&tmp, disk)?;
        return Ok(());
    }
    let tmp = disk.with_extension("tmp");
    fs::copy(base, &tmp).context("copying base image")?;
    fs::OpenOptions::new().write(true).open(&tmp)?.set_len(DISK_SIZE)?;
    fs::rename(&tmp, disk)?;
    Ok(())
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
