//! The VM's files in Finder at ~/Berth (src/files.rs mounts it): Docker
//! volumes, machines, and running containers' root filesystems, shared
//! over SMB to the Mac only.
//!
//! SMB rather than the kernel's NFS server: containers' root filesystems
//! are overlay mounts, which knfsd can't export, while Samba reads them like
//! any folder. It also checks a password the host generates (only the Mac
//! user can read it), instead of trusting whoever connects.
//!
//! /srv/berth holds bind mounts of /var/lib/docker/volumes and
//! /var/lib/machines, and `containers/<name>`: relative symlinks into
//! `.rootfs`, a mirror of where Docker mounts containers' root filesystems.
//! The mirror is a recursive bind made a slave of the original, so each
//! container's mount appears in it when Docker mounts it and goes when
//! Docker unmounts it. A bind of each root filesystem instead would keep a
//! stopped container's overlay alive, and its restart would mount the same
//! upper directory twice.
//!
//! Links are only followed inside the share (`wide links = no`): one that
//! a container or machine planted, pointing elsewhere in the VM (`/etc`,
//! `/dev`), is refused rather than served as root. Everything is served as
//! root: the Mac user can open what containers and machines wrote, whoever
//! owns it in the VM.

use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

const ROOT: &str = "/srv/berth";
const CONF: &str = "/etc/samba/smb.conf";
const USER: &str = "berthsmb";
/// The share (src/files.rs mounts smb://<vm>/berth).
pub const SHARE: &str = "berth";

/// Set once the share is up: container links are kept from then on.
static ENABLED: AtomicBool = AtomicBool::new(false);

fn sh(script: &str) -> io::Result<String> {
    let out = Command::new("sh").args(["-c", script]).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(err.lines().last().unwrap_or("command failed").to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn mounted(path: &str) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo").is_ok_and(|info| info.lines().any(|l| l.split(' ').nth(4) == Some(path)))
}

/// `files <password>`: make sure the share is up with this password.
pub fn ensure(password: &str) -> io::Result<()> {
    if password.len() < 32 || !password.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::other("bad password"));
    }
    if !Path::new("/usr/sbin/smbd").exists() {
        eprintln!("berth-agent: files: installing Samba");
        sh("DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends samba >/dev/null")?;
        // Only smbd: no NetBIOS name service, no AD domain controller.
        let _ = sh("systemctl disable --now nmbd samba-ad-dc 2>/dev/null; true");
    }
    // An earlier berth exported these over NFS.
    if Path::new("/etc/exports.d/berth.exports").exists() {
        let _ = std::fs::remove_file("/etc/exports.d/berth.exports");
        let _ = sh("exportfs -ra 2>/dev/null; systemctl disable --now nfs-server 2>/dev/null; true");
    }
    // Recursive (a volume can be a mount of its own) and slaves (mounts and
    // unmounts there show up here, ours don't go back).
    for (name, source) in [
        ("volumes", "/var/lib/docker/volumes"),
        ("machines", "/var/lib/machines"),
        (".rootfs", "/var/lib/docker/rootfs/overlayfs"),
        (".overlay2", "/var/lib/docker/overlay2"),
    ] {
        let target = format!("{ROOT}/{name}");
        std::fs::create_dir_all(&target)?;
        std::fs::create_dir_all(source)?;
        if !mounted(&target) {
            sh(&format!("mount --rbind {source} {target} && mount --make-rslave {target}"))?;
        }
    }
    std::fs::create_dir_all(format!("{ROOT}/containers"))?;
    // Keeps macOS from indexing the share when it's mounted: that walked
    // every volume (a kernel source tree is millions of files), held ~16k
    // directories open and left smbd with a 600 MB heap at each start.
    std::fs::write(format!("{ROOT}/.metadata_never_index"), "")?;

    // The Mac is the VM's default gateway on the NAT network; listen only
    // on the address it reaches us at.
    let mac = sh("ip -4 route show default | awk '{print $3; exit}'")?.trim().to_string();
    let dev = sh("ip -4 route show default | awk '{print $5; exit}'")?.trim().to_string();
    if mac.parse::<std::net::Ipv4Addr>().is_err() || dev.is_empty() || !dev.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(io::Error::other("no default route"));
    }
    let conf = format!(
        "# Written by berth (agent/src/files.rs).\n\
         [global]\n\
         \x20 server role = standalone server\n\
         \x20 map to guest = never\n\
         \x20 disable netbios = yes\n\
         \x20 smb ports = 445\n\
         \x20 interfaces = {dev}\n\
         \x20 bind interfaces only = yes\n\
         \x20 hosts allow = {mac}\n\
         \x20 hosts deny = ALL\n\
         \x20 server min protocol = SMB3\n\
         \x20 load printers = no\n\
         \x20 printing = bsd\n\
         \x20 printcap name = /dev/null\n\
         \x20 disable spoolss = yes\n\
         \x20 log level = 0\n\
         \x20 server signing = mandatory\n\
         \x20 vfs objects = fruit streams_xattr\n\
         \x20 fruit:metadata = stream\n\
         \x20 fruit:nfs_aces = no\n\
         [{SHARE}]\n\
         \x20 path = {ROOT}\n\
         \x20 valid users = {USER}\n\
         \x20 read only = no\n\
         \x20 force user = root\n\
         \x20 force group = root\n\
         \x20 follow symlinks = yes\n\
         \x20 wide links = no\n\
         \x20 hide files = /.rootfs/.overlay2/\n\
         \x20 case sensitive = yes\n"
    );
    let changed = std::fs::read_to_string(CONF).ok().as_deref() != Some(conf.as_str());
    if changed {
        std::fs::write(CONF, &conf)?;
    }
    if sh(&format!("id -u {USER}")).is_err() {
        sh(&format!("useradd --system --no-create-home --shell /usr/sbin/nologin {USER}"))?;
    }
    // The password goes on stdin, not the command line.
    let mut child = Command::new("smbpasswd")
        .args(["-a", "-s", USER])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        io::Write::write_all(&mut stdin, format!("{password}\n{password}\n").as_bytes())?;
    }
    if !child.wait()?.success() {
        return Err(io::Error::other("smbpasswd failed"));
    }
    if changed || sh("systemctl is-active -q smbd").is_err() {
        sh("systemctl enable -q smbd && systemctl restart smbd")?;
    }
    ENABLED.store(true, Ordering::SeqCst);
    sync_containers();
    Ok(())
}

/// Keep `containers/<name>` pointing at each running container's root
/// filesystem (called from the domain sync, which follows Docker events).
pub fn sync_containers() {
    if !ENABLED.load(Ordering::SeqCst) {
        return;
    }
    let dir = format!("{ROOT}/containers");
    let mut wanted = std::collections::BTreeMap::new();
    for c in crate::docker::containers().unwrap_or_default() {
        let Some(name) = c.name().filter(|n| !n.is_empty() && !n.contains('/') && !n.starts_with('.')) else { continue };
        if let Some(rootfs) = rootfs(&c.id) {
            // First come keeps a name (Docker and nerdctl can share one).
            wanted.entry(name.to_string()).or_insert(rootfs);
        }
    }
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let target = std::fs::read_link(entry.path()).ok();
        if target.as_ref().map(|t| t.to_string_lossy().into_owned()) != wanted.get(&name).cloned() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    for (name, rootfs) in wanted {
        let link = format!("{dir}/{name}");
        if std::fs::symlink_metadata(&link).is_err() {
            let _ = std::os::unix::fs::symlink(&rootfs, &link);
        }
    }
}

/// A running container's root filesystem, as a link target relative to
/// `containers/`, inside the share's mirrors: containerd's image store
/// (Docker 29's default), or overlay2's merged dir.
fn rootfs(id: &str) -> Option<String> {
    if !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if mounted(&format!("{ROOT}/.rootfs/{id}")) {
        return Some(format!("../.rootfs/{id}"));
    }
    let inspect = crate::docker::inspect(id).ok()?;
    let merged = inspect["GraphDriver"]["Data"]["MergedDir"].as_str()?;
    let layer = merged.strip_prefix("/var/lib/docker/overlay2/")?.strip_suffix("/merged")?;
    let mirrored = format!("{ROOT}/.overlay2/{layer}/merged");
    (layer.bytes().all(|b| b.is_ascii_alphanumeric()) && mounted(&mirrored)).then(|| format!("../.overlay2/{layer}/merged"))
}
