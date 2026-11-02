//! A macOS VM's files under `~/.berth/mac/<name>/` (docs/gui-protocol.md,
//! "Transport"): its identity (`hw.json`), settings (`vm.json`), the
//! `run.pid` lock that says it's running, and the APFS clones that make
//! `clone` and `reset` instant. Pure file handling, no Virtualization.framework,
//! so it's unit-tested.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// Apple's macOS license allows two macOS VMs running at once on a Mac.
pub const MAX_RUNNING: usize = 2;

/// `~/.berth/mac`: one directory per VM, plus the IPSW cache.
pub fn root(paths: &Paths) -> PathBuf {
    paths.root.join("mac")
}

/// One VM's directory and the files in it.
#[derive(Clone, Debug)]
pub struct Vm {
    pub name: String,
    pub dir: PathBuf,
}

impl Vm {
    pub fn new(root: &Path, name: &str) -> Self {
        Self { name: name.to_string(), dir: root.join(name) }
    }
    pub fn disk(&self) -> PathBuf {
        self.dir.join("disk.img")
    }
    pub fn aux(&self) -> PathBuf {
        self.dir.join("aux.img")
    }
    pub fn hw(&self) -> PathBuf {
        self.dir.join("hw.json")
    }
    pub fn meta(&self) -> PathBuf {
        self.dir.join("vm.json")
    }
    pub fn pid(&self) -> PathBuf {
        self.dir.join("run.pid")
    }
    pub fn control(&self) -> PathBuf {
        self.dir.join("control.sock")
    }
    pub fn agent(&self) -> PathBuf {
        self.dir.join("agent.sock")
    }
    /// The VM process's stdout and stderr.
    pub fn log(&self) -> PathBuf {
        self.dir.join("run.log")
    }
    /// Shared into the guest for `berth gui push` (see vz.rs).
    pub fn push(&self) -> PathBuf {
        self.dir.join("push")
    }
    /// The account to create on the first boot after install; removed once
    /// that boot has started (macOS only reads it then).
    pub fn provision(&self) -> PathBuf {
        self.dir.join("provision.json")
    }
    /// The clean state `set-base` saved: what clones are made from and what
    /// `reset` of this VM itself goes back to.
    pub fn clean(&self) -> PathBuf {
        self.dir.join("base")
    }
    pub fn exists(&self) -> bool {
        self.meta().exists()
    }
    pub fn has_clean(&self) -> bool {
        self.clean().join("disk.img").exists() && self.clean().join("aux.img").exists()
    }

    pub fn load_meta(&self) -> Result<Meta> {
        let bytes = fs::read(self.meta()).with_context(|| format!("no macOS VM named {}", self.name))?;
        serde_json::from_slice(&bytes).with_context(|| format!("{} isn't valid", self.meta().display()))
    }
    pub fn save_meta(&self, meta: &Meta) -> Result<()> {
        write_atomic(&self.meta(), &serde_json::to_vec_pretty(meta)?)
    }
    pub fn load_hw(&self) -> Result<Hardware> {
        let bytes = fs::read(self.hw()).with_context(|| format!("reading {}", self.hw().display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("{} isn't valid", self.hw().display()))
    }
    pub fn save_hw(&self, hw: &Hardware) -> Result<()> {
        write_atomic(&self.hw(), &serde_json::to_vec_pretty(hw)?)
    }

    /// The VM process's pid while it runs: it holds an flock on `run.pid`
    /// for its lifetime, so a pid left behind by a crash doesn't count and a
    /// reused pid can't be mistaken for it.
    pub fn running_pid(&self) -> Option<u32> {
        let file = fs::File::open(self.pid()).ok()?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
            return None;
        }
        fs::read_to_string(self.pid()).ok()?.trim().parse().ok()
    }
    pub fn is_running(&self) -> bool {
        self.running_pid().is_some()
    }

    /// Take `run.pid` for this process; the returned file holds the lock.
    /// A checker holds the lock for an instant, so a few tries.
    pub fn lock_pid(&self) -> Result<fs::File> {
        let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.pid())?;
        for _ in 0..20 {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                file.set_len(0)?;
                std::io::Write::write_all(&mut &file, std::process::id().to_string().as_bytes())?;
                return Ok(file);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        bail!("macOS VM {} is already running", self.name)
    }
}

/// `vm.json`. `cpus`, `memory_mib`, `base` and `created` are the protocol's;
/// the rest is berth's own and optional, so older files still load.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Meta {
    pub cpus: usize,
    pub memory_mib: u64,
    /// The VM this one was cloned from, which `reset` goes back to.
    pub base: Option<String>,
    /// Unix seconds.
    pub created: u64,
    /// The account made at install, for `berth mac shell`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default)]
    pub display: Display,
    /// The macOS build installed, e.g. "25A354".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

/// The guest's one display, in pixels.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub ppi: u32,
}

impl Default for Display {
    fn default() -> Self {
        Self::parse(DEFAULT_SCREEN).unwrap()
    }
}

pub const DEFAULT_SCREEN: &str = "1920x1200@2x";

impl Display {
    /// `WIDTHxHEIGHT[@SCALEx]` in points, as `--screen` takes it: `@2x` is a
    /// Retina display (twice the pixels each way, ~220 ppi, which macOS shows
    /// as HiDPI), `@1x` a plain one.
    pub fn parse(s: &str) -> Result<Self> {
        let (size, scale) = match s.split_once('@') {
            Some((size, scale)) => (size, scale.trim_end_matches('x')),
            None => (s, "1"),
        };
        let scale: u32 = match scale.parse() {
            Ok(n @ (1 | 2)) => n,
            _ => bail!("screen scale is @1x or @2x"),
        };
        let (w, h) = size.split_once('x').context("screen size is WIDTHxHEIGHT, e.g. 1920x1200@2x")?;
        let (w, h): (u32, u32) = (w.parse().context("bad screen width")?, h.parse().context("bad screen height")?);
        if !(640..=4096).contains(&w) || !(480..=4096).contains(&h) {
            bail!("screen size is between 640x480 and 4096x4096 points");
        }
        Ok(Self { width: w * scale, height: h * scale, ppi: 110 * scale })
    }

    pub fn scale(&self) -> u32 {
        if self.ppi >= 200 { 2 } else { 1 }
    }
}

impl std::fmt::Display for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.scale();
        write!(f, "{}x{}@{s}x", self.width / s, self.height / s)
    }
}

/// `hw.json`: what makes the VM the same Mac every boot. The hardware model
/// comes from the IPSW it was installed from and is shared with its clones;
/// the machine identifier (the VM's ECID-like serial) and MAC address are
/// its own, new for every clone.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Hardware {
    /// `VZMacHardwareModel.dataRepresentation`, base64.
    pub hardware_model: String,
    /// `VZMacMachineIdentifier.dataRepresentation`, base64.
    pub machine_identifier: String,
    /// The network device's, so its DHCP lease (and `shell`) can be found.
    pub mac_address: String,
}

impl Hardware {
    pub fn new(hardware_model: &[u8], machine_identifier: &[u8], mac_address: &str) -> Self {
        Self {
            hardware_model: base64_encode(hardware_model),
            machine_identifier: base64_encode(machine_identifier),
            mac_address: mac_address.to_string(),
        }
    }
    pub fn hardware_model_bytes(&self) -> Result<Vec<u8>> {
        base64_decode(&self.hardware_model).context("hw.json: bad hardware_model")
    }
    pub fn machine_identifier_bytes(&self) -> Result<Vec<u8>> {
        base64_decode(&self.machine_identifier).context("hw.json: bad machine_identifier")
    }
}

/// VM names become directory names and appear in commands.
pub fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with(['.', '-'])
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("macOS VM names use letters, digits, '-', '_' and '.' (up to 64, not starting with '.' or '-')");
    }
    Ok(())
}

/// Every VM, by name.
pub fn all(root: &Path) -> Vec<Vm> {
    let mut vms: Vec<Vm> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_name(n).is_ok())
        .map(|n| Vm::new(root, &n))
        .filter(Vm::exists)
        .collect();
    vms.sort_by(|a, b| a.name.cmp(&b.name));
    vms
}

/// The VMs whose process is alive.
pub fn running_in(root: &Path) -> Vec<String> {
    all(root).into_iter().filter(Vm::is_running).map(|vm| vm.name).collect()
}

/// Whether `name` may start with `running` already up.
pub fn check_limit(running: &[String], name: &str) -> Result<()> {
    if running.iter().any(|r| r == name) || running.len() < MAX_RUNNING {
        return Ok(());
    }
    bail!(
        "{} macOS VMs are running ({}), and macOS allows {MAX_RUNNING} at once; stop one first: berth mac stop <name>",
        running.len(),
        running.join(", ")
    )
}

/// Copy-on-write copy (instant on APFS), else a plain copy. Written next to
/// `dst` and renamed, so `dst` is either the old file or the whole new one.
pub fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let tmp = dst.with_extension("cloning");
    let _ = fs::remove_file(&tmp);
    let (s, d) = (CString::new(src.as_os_str().as_bytes())?, CString::new(tmp.as_os_str().as_bytes())?);
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } != 0 {
        fs::copy(src, &tmp).with_context(|| format!("copying {}", src.display()))?;
    }
    fs::rename(&tmp, dst)?;
    Ok(())
}

/// Bytes a file really takes (sparse and ASIF images are mostly holes).
pub fn allocated(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).map_or(0, |m| m.blocks() * 512)
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (as NSData's own), for the identity blobs.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("berth-macvm-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn base64_round_trip() {
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_decode("TQ==").unwrap(), b"M");
        assert!(base64_decode("not base64!").is_none());
    }

    #[test]
    fn hw_json_round_trip() {
        let root = temp_root("hw");
        let vm = Vm::new(&root, "a");
        fs::create_dir_all(&vm.dir).unwrap();
        let hw = Hardware::new(&[1, 2, 3, 250, 0], &[9; 33], "a2:b3:04:05:06:07");
        vm.save_hw(&hw).unwrap();
        let back = vm.load_hw().unwrap();
        assert_eq!(back, hw);
        assert_eq!(back.hardware_model_bytes().unwrap(), vec![1, 2, 3, 250, 0]);
        assert_eq!(back.machine_identifier_bytes().unwrap(), vec![9; 33]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn vm_json_round_trip_and_protocol_fields() {
        let root = temp_root("meta");
        let vm = Vm::new(&root, "a");
        fs::create_dir_all(&vm.dir).unwrap();
        let meta = Meta {
            cpus: 4,
            memory_mib: 8192,
            base: None,
            created: 1_790_000_000,
            user: Some("berth".into()),
            display: Display::default(),
            build: None,
        };
        vm.save_meta(&meta).unwrap();
        assert_eq!(vm.load_meta().unwrap(), meta);
        // The protocol's fields are always there, base as null.
        let v: serde_json::Value = serde_json::from_slice(&fs::read(vm.meta()).unwrap()).unwrap();
        assert_eq!(v["base"], serde_json::Value::Null);
        assert_eq!(v["cpus"], 4);
        assert_eq!(v["memory_mib"], 8192);
        // A minimal file (only the protocol's fields) loads with defaults.
        fs::write(vm.meta(), r#"{"cpus":2,"memory_mib":4096,"base":"gold","created":5}"#).unwrap();
        let m = vm.load_meta().unwrap();
        assert_eq!(m.base.as_deref(), Some("gold"));
        assert_eq!(m.display, Display::default());
        assert!(m.user.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn screen_sizes() {
        let d = Display::parse("1920x1200@2x").unwrap();
        assert_eq!((d.width, d.height, d.ppi), (3840, 2400, 220));
        assert_eq!(d.to_string(), "1920x1200@2x");
        let d = Display::parse("1440x900").unwrap();
        assert_eq!((d.width, d.height, d.scale()), (1440, 900, 1));
        assert_eq!(Display::parse("1280x800@1x").unwrap().to_string(), "1280x800@1x");
        assert!(Display::parse("1920x1200@3x").is_err());
        assert!(Display::parse("10x10").is_err());
        assert!(Display::parse("big").is_err());
    }

    #[test]
    fn names() {
        for ok in ["test", "gui-test-1", "a.b_c"] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "-x", "a/b", "a b", &"x".repeat(65)] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn limit_counts_running_vms() {
        let none: Vec<String> = vec![];
        assert!(check_limit(&none, "a").is_ok());
        assert!(check_limit(&["a".into()], "b").is_ok());
        let two = vec!["a".to_string(), "b".to_string()];
        // Starting one that is already running is not a third.
        assert!(check_limit(&two, "a").is_ok());
        let err = check_limit(&two, "c").unwrap_err().to_string();
        assert!(err.contains("a, b"), "{err}");
    }

    #[test]
    fn running_counts_live_locks_only() {
        let root = temp_root("running");
        let meta = Meta { cpus: 2, memory_mib: 4096, base: None, created: 0, user: None, display: Display::default(), build: None };
        let vms: Vec<Vm> = ["a", "b", "c"].iter().map(|n| Vm::new(&root, n)).collect();
        for vm in &vms {
            fs::create_dir_all(&vm.dir).unwrap();
            vm.save_meta(&meta).unwrap();
        }
        // A directory without vm.json isn't a VM.
        fs::create_dir_all(root.join("cache")).unwrap();
        assert!(running_in(&root).is_empty());
        // "a" is up (holds its lock); "b" left a pid file behind after a crash.
        let lock = vms[0].lock_pid().unwrap();
        fs::write(vms[1].pid(), "4242").unwrap();
        assert_eq!(running_in(&root), vec!["a".to_string()]);
        assert_eq!(vms[0].running_pid(), Some(std::process::id()));
        assert!(vms[1].running_pid().is_none());
        // A second process (here: open file) can't take a held lock.
        assert!(vms[0].lock_pid().is_err());
        let lock_b = vms[1].lock_pid().unwrap();
        assert_eq!(running_in(&root), vec!["a".to_string(), "b".to_string()]);
        assert!(check_limit(&running_in(&root), "c").is_err());
        drop(lock);
        assert_eq!(running_in(&root), vec!["b".to_string()]);
        assert!(check_limit(&running_in(&root), "c").is_ok());
        drop(lock_b);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn clone_file_copies() {
        let root = temp_root("clone");
        let (a, b) = (root.join("a.img"), root.join("b.img"));
        fs::write(&a, b"disk").unwrap();
        fs::write(&b, b"old").unwrap();
        clone_file(&a, &b).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"disk");
        assert!(!b.with_extension("cloning").exists());
        let _ = fs::remove_dir_all(root);
    }
}
