//! The VM's files in Finder: Docker volumes and machines, exported over
//! NFS to the Mac only, which mounts them at ~/Berth (src/files.rs).
//!
//! The export root /srv/berth holds bind mounts of /var/lib/docker/volumes
//! and /var/lib/machines. Requests are squashed to root: the Mac user can
//! open what containers and machines wrote, whoever owns it in the VM. Only
//! the Mac's address on the VM network may mount it, from a reserved port
//! (root on the Mac: berth's helper).

use std::io;
use std::path::Path;
use std::process::Command;

const ROOT: &str = "/srv/berth";
const EXPORTS: &str = "/etc/exports.d/berth.exports";
const NFS_CONF: &str = "/etc/nfs.conf.d/berth.conf";
/// Fixed so the Mac can mount without asking rpcbind (src/helper.rs).
pub const MOUNTD_PORT: u16 = 20048;

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

/// `files` header: make sure the export is up; replies "ok" (or an error).
pub fn ensure() -> io::Result<()> {
    if !Path::new("/usr/sbin/exportfs").exists() {
        eprintln!("berth-agent: files: installing the NFS server");
        sh("DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends nfs-kernel-server >/dev/null")?;
    }
    for (name, source) in [("volumes", "/var/lib/docker/volumes"), ("machines", "/var/lib/machines")] {
        let target = format!("{ROOT}/{name}");
        std::fs::create_dir_all(&target)?;
        std::fs::create_dir_all(source)?;
        if !mounted(&target) {
            sh(&format!("mount --bind {source} {target}"))?;
        }
    }
    // The Mac is the VM's default gateway on the NAT network.
    let mac = sh("ip -4 route show default | awk '{print $3; exit}'")?.trim().to_string();
    if mac.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(io::Error::other("no default gateway"));
    }
    // `secure` (the default): reserved source ports only, which on the Mac
    // only root has: its helper mounts it (src/helper.rs).
    let exports = format!("{ROOT} {mac}(rw,secure,no_subtree_check,crossmnt,all_squash,anonuid=0,anongid=0)\n");
    let conf = format!("[mountd]\nport={MOUNTD_PORT}\n[nfsd]\nthreads=8\n");
    let mut changed = false;
    for (path, contents) in [(EXPORTS, &exports), (NFS_CONF, &conf)] {
        if std::fs::read_to_string(path).ok().as_deref() != Some(contents.as_str()) {
            std::fs::create_dir_all(Path::new(path).parent().unwrap_or(Path::new("/")))?;
            std::fs::write(path, contents)?;
            changed = true;
        }
    }
    if changed || sh("systemctl is-active -q nfs-server").is_err() {
        sh("systemctl enable -q nfs-server && systemctl restart nfs-server")?;
    }
    sh("exportfs -ra")?;
    Ok(())
}
