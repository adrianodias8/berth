//! ~/Berth in Finder: the VM's Docker volumes and machines, mounted over
//! NFS from the VM (agent/src/files.rs). Mounted once the agent is up,
//! unmounted before the VM stops: an NFS mount whose server went away hangs
//! whatever touches it. Off with `"finder": false` in config.json.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::AsyncReadExt;

use crate::paths::Paths;

/// agent/src/files.rs MOUNTD_PORT.
const MOUNTD_PORT: u16 = 20048;

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
    std::fs::create_dir_all(&dir)?;
    // Soft and interruptible: if the VM goes away first, calls fail instead
    // of hanging Finder. No locking: NFSv3's lock daemon isn't run.
    let options = format!("vers=3,tcp,port=2049,mountport={MOUNTD_PORT},nolocks,locallocks,soft,intr,timeo=20,retrans=2,rdirplus,acdirmax=3");
    let out = tokio::process::Command::new("/sbin/mount_nfs")
        .args(["-o", &options, &format!("{ip}:/srv/berth")])
        .arg(&dir)
        .output()
        .await?;
    if !out.status.success() {
        bail!("mount_nfs: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    eprintln!("files: the VM's volumes and machines are in {}", dir.display());
    Ok(())
}

/// Before the VM stops. Forced: open files in Finder mustn't keep it busy.
pub fn unmount(paths: &Paths) {
    let dir = dir(paths);
    if mounted_type(&dir).as_deref() != Some("nfs") {
        return;
    }
    let mut child = match std::process::Command::new("/sbin/umount").arg("-f").arg(&dir).spawn() {
        Ok(child) => child,
        Err(_) => return,
    };
    // Don't let a wedged unmount hold up stopping.
    for _ in 0..50 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
}
