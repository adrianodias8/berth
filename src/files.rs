//! ~/Berth in Finder: the VM's Docker volumes, machines and running
//! containers' root filesystems, mounted over SMB from the VM
//! (agent/src/files.rs). Mounted once the agent is up, unmounted before the
//! VM stops. Off with `"finder": false` in config.json.
//!
//! The share checks a random password kept in ~/.berth/files-password (only
//! the Mac user can read it), which macOS's NetFS gets directly: never on a
//! command line, where other processes could read it.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString, NSURL};
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

/// What's mounted at `dir`, if anything ("smbfs", "nfs"; "apfs" for nothing).
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

/// The share's password, made on first use.
fn password(paths: &Paths) -> Result<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = paths.root.join("files-password");
    if let Ok(existing) = std::fs::read_to_string(&path)
        && existing.trim().len() == 64
    {
        return Ok(existing.trim().to_string());
    }
    let mut bytes = [0u8; 32];
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    let new: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    // A new file with 0600 from the start, then renamed over: never a moment
    // with the password readable, nor half written.
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    file.write_all(new.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(new)
}

pub async fn mount(paths: &Paths) -> Result<()> {
    let dir = dir(paths);
    if !enabled(paths) || mounted_type(&dir).as_deref() == Some("smbfs") {
        return Ok(());
    }
    let password = password(paths)?;
    let mut guest = crate::daemon::guest_stream(&format!("files {password}")).await?;
    let mut reply = String::new();
    guest.read_to_string(&mut reply).await?;
    if reply.trim() != "ok" {
        bail!("the VM couldn't share its files: {}", reply.trim());
    }
    let Some(ip) = *crate::routes::GUEST_IP.lock().unwrap() else { bail!("no VM address yet") };
    if STOPPED.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let target = dir.clone();
    // Foundation objects on a thread without a pool of its own.
    tokio::task::spawn_blocking(move || objc2::rc::autoreleasepool(|_| netfs_mount(ip, &target, &password))).await??;
    eprintln!("files: the VM's volumes, machines and containers are in {}", dir.display());
    Ok(())
}

#[link(name = "NetFS", kind = "framework")]
unsafe extern "C" {
    /// NetFS.h: CFURLRef, CFURLRef, CFStringRef, CFStringRef,
    /// CFMutableDictionaryRef, CFMutableDictionaryRef, CFArrayRef * (all
    /// toll-free bridged with their Foundation classes).
    fn NetFSMountURLSync(
        url: *const AnyObject,
        mountpath: *const AnyObject,
        user: *const AnyObject,
        passwd: *const AnyObject,
        open_options: *const AnyObject,
        mount_options: *const AnyObject,
        mountpoints: *mut *mut AnyObject,
    ) -> i32;
}

/// smb://berthsmb@<vm>/berth at exactly `dir`: no UI, soft (a VM that went
/// away fails calls instead of hanging Finder), not in Finder's recents.
fn netfs_mount(ip: std::net::Ipv4Addr, dir: &std::path::Path, password: &str) -> Result<()> {
    let url = NSURL::URLWithString(&NSString::from_str(&format!("smb://{ip}/berth"))).context("share URL")?;
    let mountpath = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
    let (user, passwd) = (NSString::from_str("berthsmb"), NSString::from_str(password));
    let yes = NSNumber::new_bool(true);
    let open: Retained<NSMutableDictionary<NSString, AnyObject>> = NSMutableDictionary::new();
    let mount: Retained<NSMutableDictionary<NSString, AnyObject>> = NSMutableDictionary::new();
    let set = |dict: &NSMutableDictionary<NSString, AnyObject>, key: &str, value: &AnyObject| unsafe {
        dict.setObject_forKey(value, objc2::runtime::ProtocolObject::from_ref(&*NSString::from_str(key)));
    };
    set(&open, "UIOption", &NSString::from_str("NoUI"));
    set(&open, "NoUserPreferences", &yes);
    set(&mount, "MountAtMountDir", &yes);
    set(&mount, "SoftMount", &yes);
    let ptr = |o: &AnyObject| o as *const AnyObject;
    let mut points: *mut AnyObject = std::ptr::null_mut();
    let status = unsafe {
        NetFSMountURLSync(ptr(&url), ptr(&mountpath), ptr(&user), ptr(&passwd), ptr(&open), ptr(&mount), &mut points)
    };
    if !points.is_null() {
        // We own the returned array (a Create-rule CFArray).
        drop(unsafe { Retained::from_raw(points) });
    }
    if status != 0 {
        bail!("mounting the VM's share: error {status}");
    }
    Ok(())
}

/// Before the VM stops: unmount, and mount no more.
pub fn unmount_for_stop(paths: &Paths) {
    STOPPED.store(true, std::sync::atomic::Ordering::SeqCst);
    unmount(paths);
}

/// Forced: open files in Finder mustn't keep it busy. Bounded: stopping
/// mustn't wait on it. (Also takes down an NFS mount from an older berth,
/// through the helper that made it.)
pub fn unmount(paths: &Paths) {
    let dir = dir(paths);
    match mounted_type(&dir).as_deref() {
        Some("smbfs") => {}
        Some("nfs") => {
            let _ = crate::helper::mount_files(None);
            return;
        }
        _ => return,
    }
    let Ok(mut child) = std::process::Command::new("/sbin/umount").arg("-f").arg(&dir).spawn() else { return };
    for _ in 0..50 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
}
