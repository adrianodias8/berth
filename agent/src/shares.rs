//! Mac folders besides home, at the same paths in the VM (the host's list
//! is virtiofs::EXTRA_SHARES): /Volumes, /private/tmp, /private/var/folders.
//! Mounted after boot, not from fstab: a missing one costs bind mounts from
//! there, not the boot. berth's own device takes the polling driver when the
//! host presents it, else the built-in one; Apple's share is plain virtiofs.

const SHARES: [(&str, &str); 3] =
    [("berth-volumes", "/Volumes"), ("berth-tmp", "/private/tmp"), ("berth-folders", "/private/var/folders")];

pub fn mount_all() {
    let _ = std::process::Command::new("modprobe").args(["-q", "virtiofs_poll"]).status();
    for (tag, dir) in SHARES {
        if mounted(dir) {
            continue;
        }
        let _ = std::fs::create_dir_all(dir);
        match mount(tag, dir) {
            Ok(fstype) => eprintln!("berth-agent: shares: {dir} ({fstype})"),
            Err(err) => eprintln!("berth-agent: shares: {dir}: {err}"),
        }
    }
}

/// A virtio-fs tag at `dir`, with whichever driver binds its device.
pub fn mount(tag: &str, dir: &str) -> std::io::Result<&'static str> {
    let (tag, dir) = (std::ffi::CString::new(tag)?, std::ffi::CString::new(dir)?);
    let mut last = std::io::Error::other("no driver");
    for fstype in ["virtiofs_poll", "virtiofs"] {
        let ty = std::ffi::CString::new(fstype)?;
        // Nothing set-uid or device-like from the Mac (anyone there can write /tmp).
        let flags = libc::MS_NOSUID | libc::MS_NODEV;
        if unsafe { libc::mount(tag.as_ptr(), dir.as_ptr(), ty.as_ptr(), flags, std::ptr::null()) } == 0 {
            return Ok(fstype);
        }
        last = std::io::Error::last_os_error();
    }
    Err(last)
}

/// Whether something is mounted at `dir` (field 5 of mountinfo).
pub fn mounted(dir: &str) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo").is_ok_and(|info| info.lines().any(|l| l.split(' ').nth(4) == Some(dir)))
}
