//! `berth snapshot`: save the whole VM (every container, image, volume,
//! machine and Kubernetes) and go back to it later.
//!
//! A snapshot of a running VM pauses it, saves its memory and device state
//! (Virtualization.framework's save/restore, macOS 14+) and clones its disk
//! at that same instant, then resumes. Restoring puts the disk back and
//! resumes the saved state: processes carry on where they were, databases
//! included, with memory and disk consistent. Disk clones are APFS
//! copy-on-write (clonefile), so taking one is instant and it only costs
//! space as the VM's disk changes afterwards.
//!
//! When the state can't be saved (the experimental file sharing device
//! doesn't support it) or restored (the file is from another macOS build),
//! the snapshot falls back to its disk: like booting after a power cut, so
//! containers restart per their restart policy. Snapshots don't include
//! files on the Mac (bind mounts).
//!
//! Layout: ~/.berth/snapshots/<name>/{meta.json, disk file, state.vzvmsave,
//! efi-vars, boot/}; a pending restore is ~/.berth/data/restore.json.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::vm::Resources;

const STATE: &str = "state.vzvmsave";

/// One pause/save/clone/resume at a time: a second one resuming the VM in
/// the middle would pair a saved memory with a later disk.
static LIFECYCLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meta {
    pub name: String,
    /// Seconds since the epoch.
    pub created: u64,
    /// The VM's memory and running processes are included, not just its disk.
    pub with_memory: bool,
    /// The hardware the state was saved with; a restore must match it.
    pub cpus: usize,
    pub memory: u64,
    /// File name of the disk (disk.img or disk.raw).
    pub disk: String,
    pub macos_build: String,
    /// Docker containers running when it was taken: restarted after a
    /// disk-only restore, where only restart policies would bring them back.
    #[serde(default)]
    pub running: Vec<String>,
    /// The home share it was saved with (virtiofs::HomeShare::as_str).
    #[serde(default)]
    pub share: Option<String>,
}

fn root(paths: &Paths) -> PathBuf {
    paths.root.join("snapshots")
}

fn restore_marker(paths: &Paths) -> PathBuf {
    paths.data().join("restore.json")
}

pub fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("snapshot names use letters, digits, '-', '_' and '.' (up to 64, not starting with '.')");
    }
    Ok(())
}

/// Copy-on-write copy (instant on APFS), else a plain copy.
fn clone(src: &Path, dst: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let _ = fs::remove_file(dst);
    let (s, d) = (CString::new(src.as_os_str().as_bytes())?, CString::new(dst.as_os_str().as_bytes())?);
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } != 0 {
        fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    let _ = fs::remove_dir_all(dst);
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        if entry.file_type()?.is_file() {
            clone(&entry.path(), &dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn macos_build() -> String {
    std::process::Command::new("sw_vers")
        .arg("-buildVersion")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The VM's files as they are now: disk, EFI variables, direct-boot files.
fn save_files(paths: &Paths, dir: &Path) -> Result<String> {
    let disk = paths.disk();
    // Writes the VM made are in the file's page cache; clone what's on disk.
    fs::File::open(&disk)?.sync_all()?;
    let name = disk.file_name().context("disk path")?.to_string_lossy().into_owned();
    clone(&disk, &dir.join(&name))?;
    if paths.efi_vars().exists() {
        clone(&paths.efi_vars(), &dir.join("efi-vars"))?;
    }
    let boot = paths.guest().join("boot");
    if boot.is_dir() {
        copy_dir(&boot, &dir.join("boot"))?;
    }
    Ok(name)
}

fn finish(paths: &Paths, partial: &Path, meta: &Meta) -> Result<()> {
    fs::write(partial.join("meta.json"), serde_json::to_vec_pretty(meta)?)?;
    fs::rename(partial, root(paths).join(&meta.name))?;
    Ok(())
}

fn new_partial(paths: &Paths, name: &str) -> Result<PathBuf> {
    valid_name(name)?;
    if root(paths).join(name).exists() {
        bail!("a snapshot named {name} already exists");
    }
    let partial = root(paths).join(format!(".{name}.partial"));
    let _ = fs::remove_dir_all(&partial);
    fs::create_dir_all(&partial)?;
    Ok(partial)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Snapshot the running VM (in the daemon). Returns what was saved.
pub async fn create_running(paths: &Paths, name: &str, with_memory: bool, res: &Resources) -> Result<Meta> {
    let _lifecycle = LIFECYCLE.lock().await;
    if crate::daemon::stopping() {
        bail!("berth is stopping");
    }
    let partial = new_partial(paths, name)?;
    let running = crate::ports::running_containers().await.unwrap_or_default();
    let mut paused = false;
    let result = async {
        let mut saved_memory = false;
        if with_memory {
            crate::vm::pause().await?;
            paused = true;
            match crate::vm::save_state(partial.join(STATE)).await {
                Ok(()) => saved_memory = true,
                Err(err) => {
                    eprintln!("snapshot {name}: can't save the VM's state ({err:#}); saving its disk only");
                    let _ = fs::remove_file(partial.join(STATE));
                    crate::vm::resume().await?;
                    paused = false;
                }
            }
        }
        if !saved_memory {
            // Disk only: flush the guest's caches first, then hold it still.
            if let Ok(mut guest) = crate::daemon::guest_stream("sync").await {
                use tokio::io::AsyncReadExt;
                let _ = guest.read_to_end(&mut Vec::new()).await;
            }
            crate::vm::pause().await?;
            paused = true;
        }
        let disk = save_files(paths, &partial);
        crate::vm::resume().await?;
        paused = false;
        Ok::<_, anyhow::Error>(Meta {
            name: name.to_string(),
            created: now(),
            with_memory: saved_memory,
            cpus: res.cpus,
            memory: res.memory,
            disk: disk?,
            macos_build: macos_build(),
            running,
            share: crate::virtiofs::CURRENT.get().map(|s| s.as_str().to_string()),
        })
    }
    .await;
    match result {
        Ok(meta) => {
            finish(paths, &partial, &meta)?;
            Ok(meta)
        }
        Err(err) => {
            let _ = fs::remove_dir_all(&partial);
            // Never leave the VM paused because a snapshot failed (but don't
            // resume one we didn't pause).
            if paused {
                let _ = crate::vm::resume().await;
            }
            Err(err)
        }
    }
}

/// Snapshot a stopped VM: just its files.
pub fn create_stopped(paths: &Paths, name: &str) -> Result<Meta> {
    let partial = new_partial(paths, name)?;
    let res = Resources::load(paths);
    let meta = save_files(paths, &partial).map(|disk| Meta {
        name: name.to_string(),
        created: now(),
        with_memory: false,
        cpus: res.cpus,
        memory: res.memory,
        disk,
        macos_build: macos_build(),
        running: Vec::new(),
        share: None,
    });
    match meta {
        Ok(meta) => {
            finish(paths, &partial, &meta)?;
            Ok(meta)
        }
        Err(err) => {
            let _ = fs::remove_dir_all(&partial);
            Err(err)
        }
    }
}

pub fn list(paths: &Paths) -> Vec<Meta> {
    let mut all: Vec<Meta> = fs::read_dir(root(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| fs::read(e.path().join("meta.json")).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    all.sort_by_key(|m| m.created);
    all
}

pub fn get(paths: &Paths, name: &str) -> Result<Meta> {
    valid_name(name)?;
    let bytes = fs::read(root(paths).join(name).join("meta.json")).with_context(|| format!("no snapshot named {name}"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn delete(paths: &Paths, name: &str) -> Result<()> {
    get(paths, name)?;
    fs::remove_dir_all(root(paths).join(name))?;
    Ok(())
}

/// Space a snapshot holds on its own: what its clones no longer share
/// with the VM is unknown to us, so this is the files' allocated size.
pub fn size_on_disk(paths: &Paths, name: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fn walk(dir: &Path) -> u64 {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => walk(&e.path()),
                Ok(m) => m.blocks() * 512,
                Err(_) => 0,
            })
            .sum()
    }
    walk(&root(paths).join(name))
}

#[derive(Serialize, Deserialize)]
struct Pending {
    state: Option<PathBuf>,
    cpus: usize,
    memory: u64,
    #[serde(default)]
    start: Vec<String>,
    #[serde(default)]
    share: Option<String>,
    /// The state is the one `berth stop` saved: delete it once used.
    #[serde(default)]
    consumed: bool,
}

/// Put the snapshot's files in place of the VM's (which must be stopped) and,
/// if it has a saved state, leave word for the next start to resume it.
pub fn prepare_restore(paths: &Paths, name: &str) -> Result<Meta> {
    let meta = get(paths, name)?;
    let dir = root(paths).join(name);
    let disk = paths.data().join(&meta.disk);
    for other in ["disk.img", "disk.raw"] {
        let _ = fs::remove_file(paths.data().join(other));
    }
    clone(&dir.join(&meta.disk), &disk)?;
    if dir.join("efi-vars").exists() {
        clone(&dir.join("efi-vars"), &paths.efi_vars())?;
    }
    if dir.join("boot").is_dir() {
        copy_dir(&dir.join("boot"), &paths.guest().join("boot"))?;
    }
    // The snapshot's disk may predate the polling driver (agent/src/kmod.rs):
    // start with the built-in one until the agent confirms it again.
    let _ = fs::remove_file(paths.guest().join("kmod-ready"));
    // A suspended VM's state is for the disk just replaced.
    let _ = fs::remove_dir_all(suspend_dir(paths));
    let state = dir.join(STATE);
    let pending = Pending {
        state: (meta.with_memory && state.exists()).then_some(state),
        cpus: meta.cpus,
        memory: meta.memory,
        start: meta.running.clone(),
        share: meta.share.clone(),
        consumed: false,
    };
    fs::write(restore_marker(paths), serde_json::to_vec(&pending)?)?;
    Ok(meta)
}

/// A pending restore, for the daemon at start.
pub struct Restore {
    /// The saved state to resume, if any.
    pub state: Option<PathBuf>,
    /// With a saved state the VM must have the hardware it was saved with.
    pub resources: Resources,
    /// Containers to start if the state can't be resumed (disk only).
    pub start: Vec<String>,
    /// The share the state was saved with.
    pub share: Option<crate::virtiofs::HomeShare>,
    /// Delete `state` once the VM is up (a suspend's).
    pub consumed: bool,
}

/// Take the pending restore, if any.
pub fn take_restore(paths: &Paths) -> Option<Restore> {
    let marker = restore_marker(paths);
    let pending: Pending = serde_json::from_slice(&fs::read(&marker).ok()?).ok()?;
    let _ = fs::remove_file(&marker);
    let resources = if pending.state.is_some() {
        Resources { cpus: pending.cpus, memory: pending.memory }
    } else {
        Resources::load(paths)
    };
    let share = pending.state.as_ref().and(pending.share.as_deref()).and_then(crate::virtiofs::HomeShare::parse);
    Some(Restore { state: pending.state, resources, start: pending.start, share, consumed: pending.consumed })
}

/// Where `berth stop` keeps the VM's state for the next start.
fn suspend_dir(paths: &Paths) -> PathBuf {
    paths.data().join("suspended")
}

/// Suspend the running VM (in the daemon): pause it, save its state and
/// leave a pending restore that the next start resumes. The VM must then be
/// stopped without a guest shutdown, so its disk stays as saved.
pub async fn suspend(paths: &Paths, res: &Resources) -> Result<()> {
    let _lifecycle = LIFECYCLE.lock().await;
    let dir = suspend_dir(paths);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let running = crate::ports::running_containers().await.unwrap_or_default();
    crate::vm::pause().await?;
    if let Err(err) = crate::vm::save_state(dir.join(STATE)).await {
        let _ = fs::remove_dir_all(&dir);
        let _ = crate::vm::resume().await;
        return Err(err);
    }
    let pending = Pending {
        state: Some(dir.join(STATE)),
        cpus: res.cpus,
        memory: res.memory,
        start: running,
        share: crate::virtiofs::CURRENT.get().map(|s| s.as_str().to_string()),
        consumed: true,
    };
    fs::write(restore_marker(paths), serde_json::to_vec(&pending)?)?;
    Ok(())
}

/// The VM is suspended: the next start resumes it.
pub fn is_suspended(paths: &Paths) -> bool {
    suspend_dir(paths).join(STATE).exists() && restore_marker(paths).exists()
}

/// Forget a suspended state (after resuming it, or when it can't be).
pub fn discard_suspended(paths: &Paths) {
    let _ = fs::remove_dir_all(suspend_dir(paths));
}

/// After a restore that booted the disk: start the containers that were
/// running when the snapshot was taken.
pub async fn start_containers(ids: Vec<String>) {
    for _ in 0..300 {
        if crate::ports::running_containers().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    for id in ids {
        if let Err(err) = crate::ports::start_container(&id).await {
            eprintln!("snapshot: starting container {}: {err:#}", &id[..id.len().min(12)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("before-migration_2.1").is_ok());
        for bad in ["", ".hidden", "a/b", "../x", "with space", &"x".repeat(65)] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn stopped_snapshot_round_trip() {
        let dir = std::env::temp_dir().join(format!("berth-snap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths { home: dir.clone(), root: dir.clone() };
        paths.create_dirs().unwrap();
        fs::write(paths.data().join("disk.img"), "v1").unwrap();
        fs::create_dir_all(paths.guest().join("boot")).unwrap();
        fs::write(paths.guest().join("boot/cmdline"), "root=/dev/vda1").unwrap();

        let meta = create_stopped(&paths, "base").unwrap();
        assert!(!meta.with_memory);
        assert!(create_stopped(&paths, "base").is_err(), "names are unique");
        fs::write(paths.data().join("disk.img"), "v2: broken").unwrap();
        fs::write(paths.guest().join("boot/cmdline"), "changed").unwrap();

        prepare_restore(&paths, "base").unwrap();
        assert_eq!(fs::read_to_string(paths.data().join("disk.img")).unwrap(), "v1");
        assert_eq!(fs::read_to_string(paths.guest().join("boot/cmdline")).unwrap(), "root=/dev/vda1");
        let restore = take_restore(&paths).unwrap();
        assert!(restore.state.is_none(), "disk only");
        assert!(take_restore(&paths).is_none(), "taken once");
        // The snapshot itself is untouched by the restore.
        assert_eq!(list(&paths).len(), 1);
        delete(&paths, "base").unwrap();
        assert!(list(&paths).is_empty());
        fs::remove_dir_all(dir).ok();
    }
}
