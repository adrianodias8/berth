//! ~/Berth in Finder: the VM's Docker volumes and machines, mounted over
//! NFS from the VM (agent/src/files.rs). Mounted once the agent is up,
//! unmounted before the VM stops: an NFS mount whose server went away hangs
//! whatever touches it. Off with `"finder": false` in config.json. Needs
//! the root helper: the VM accepts mounts from reserved ports only.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::AsyncReadExt;

use crate::paths::Paths;


/// Set once the VM is on its way down: no more mounts after that.
static STOPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn dir(paths: &Paths) -> PathBuf {
    paths.home.join("Berth")
}

fn enabled(paths: &Paths) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Config {
        finder: Option<bool>,
    }
    std::fs::read(paths.config())
        .ok()
        .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
        .and_then(|c| c.finder)
        .unwrap_or(true)
}

/// What's mounted at `dir`, if anything ("nfs", "apfs" for nothing).
fn mounted_type(dir: &std::path::Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut fs) } != 0 {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(fs.f_fstypename.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

pub async fn mount(paths: &Paths) -> Result<()> {
    let dir = dir(paths);
    if !enabled(paths) {
        return Ok(());
    }
    if mounted_type(&dir).as_deref() == Some("nfs") {
        return Ok(());
    }
    let mut guest = crate::daemon::guest_stream("files").await?;
    let mut reply = String::new();
    guest.read_to_string(&mut reply).await?;
    if reply.trim() != "ok" {
        bail!("the VM couldn't export its files: {}", reply.trim());
    }
    let Some(ip) = *crate::routes::GUEST_IP.lock().unwrap() else { bail!("no VM address yet") };
    if STOPPED.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    // The VM exports only to reserved ports (or any local process could
    // mount it, as root in the VM): root mounts it, through the helper.
    // Soft and interruptible: if the VM goes away first, calls fail instead
    // of hanging Finder. No locking: NFSv3's lock daemon isn't run.
    tokio::task::spawn_blocking(move || crate::helper::mount_files(Some(ip))).await??;
    eprintln!("files: the VM's volumes and machines are in {}", dir.display());
    Ok(())
}

/// Before the VM stops: unmount, and mount no more.
pub fn unmount_for_stop(paths: &Paths) {
    STOPPED.store(true, std::sync::atomic::Ordering::SeqCst);
    unmount(paths);
}

/// Forced: open files in Finder mustn't keep it busy. Root mounted it, so
/// the helper unmounts it (bounded: stopping mustn't wait on it).
pub fn unmount(paths: &Paths) {
    if mounted_type(&dir(paths)).as_deref() != Some("nfs") {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::helper::mount_files(None));
    });
    let _ = rx.recv_timeout(Duration::from_secs(5));
}
