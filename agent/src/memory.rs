//! Guest memory reporting for the host's balloon controller (src/memory.rs).
//!
//! The host polls `info memory` to decide how far to shrink the VM. Polling is
//! too slow for a burst of allocation, so we also arm a PSI trigger: when tasks
//! stall on memory, the kernel wakes us and we tell the host to give memory
//! back right away. The trigger is event-driven, so it costs nothing at idle.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::hostnet::{HOST_VSOCK_PORT, VMADDR_CID_HOST, vsock_connect};

/// 50 ms of memory stall within any 1 s window.
const PSI_TRIGGER: &[u8] = b"some 50000 1000000\0";

#[derive(Serialize, Default, Debug, PartialEq)]
pub struct MemInfo {
    /// MemTotal. With VIRTIO_BALLOON_F_DEFLATE_ON_OOM (which VZ negotiates)
    /// this does not shrink as the balloon inflates; see `balloon`.
    pub total: u64,
    /// MemAvailable: free plus easily reclaimable (page cache etc.).
    pub available: u64,
    /// "some" avg10 from /proc/pressure/memory, in percent.
    pub pressure: f64,
    /// Bytes currently held by the balloon (counted as used in MemTotal).
    pub balloon: u64,
    /// The kernel doesn't zero pages on allocation, so inflating the balloon
    /// costs macOS nothing. False on a VM's very first boot (the setting
    /// applies from the next one); the host then leaves the balloon alone.
    pub balloon_safe: bool,
}

pub fn read() -> io::Result<MemInfo> {
    let mut info = parse_meminfo(&std::fs::read_to_string("/proc/meminfo")?);
    if let Ok(psi) = std::fs::read_to_string("/proc/pressure/memory") {
        info.pressure = parse_psi_some_avg10(&psi).unwrap_or(0.0);
    }
    info.balloon_safe = std::fs::read_to_string("/proc/cmdline").is_ok_and(|c| c.contains("init_on_alloc=0"));
    if let Ok(vmstat) = std::fs::read_to_string("/proc/vmstat") {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        info.balloon = parse_balloon_pages(&vmstat) * page;
    }
    Ok(info)
}

fn parse_meminfo(text: &str) -> MemInfo {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok())
            .map_or(0, |kib| kib * 1024)
    };
    MemInfo { total: field("MemTotal:"), available: field("MemAvailable:"), ..Default::default() }
}

/// Pages in the balloon: inflated minus deflated, from /proc/vmstat.
fn parse_balloon_pages(vmstat: &str) -> u64 {
    let counter = |name: &str| {
        vmstat.lines().find_map(|l| l.strip_prefix(name)?.trim().parse::<u64>().ok()).unwrap_or(0)
    };
    counter("balloon_inflate ").saturating_sub(counter("balloon_deflate "))
}

fn parse_psi_some_avg10(text: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|kv| kv.strip_prefix("avg10="))?
        .parse()
        .ok()
}

const GRUB_SNIPPET: &str = "/etc/default/grub.d/90-berth.cfg";

/// Make sure the kernel boots with init_on_alloc=0 (see guest/user-data.yaml).
/// VMs provisioned before this existed get it here; it applies from the next
/// restart.
pub fn ensure_kernel_args() {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if cmdline.contains("init_on_alloc=0") || std::path::Path::new(GRUB_SNIPPET).exists() {
        return;
    }
    let snippet = "GRUB_CMDLINE_LINUX_DEFAULT=\"$GRUB_CMDLINE_LINUX_DEFAULT init_on_alloc=0\"\n";
    let ok = std::fs::create_dir_all("/etc/default/grub.d").is_ok()
        && std::fs::write(GRUB_SNIPPET, snippet).is_ok()
        && std::process::Command::new("update-grub").output().is_ok_and(|o| o.status.success());
    eprintln!("berth-agent: set init_on_alloc=0 for the next boot ({})", if ok { "ok" } else { "failed" });
}

/// Memory settings for a VM that shares the Mac's memory, applied on every
/// boot (none of them persist):
/// - compressed swap in RAM (zram, as big as the VM's memory): pages nobody
///   touches are kept at a fraction of their size instead of holding memory
///   the balloon could give back to macOS;
/// - swappiness 20: reclaim prefers page cache, and swaps only cold pages;
/// - huge pages only where programs ask (madvise): with `always`, a few
///   touched bytes can pin 2 MiB the balloon can't take back.
pub fn tune() {
    let _ = std::fs::write("/proc/sys/vm/swappiness", "20");
    let _ = std::fs::write("/sys/kernel/mm/transparent_hugepage/enabled", "madvise");
    if let Err(err) = zram_swap() {
        eprintln!("berth-agent: zram swap: {err}");
    }
}

fn zram_swap() -> io::Result<()> {
    if std::fs::read_to_string("/proc/swaps")?.contains("/dev/zram") {
        return Ok(());
    }
    // Built into berth's kernel; a module in Debian's.
    let _ = std::process::Command::new("modprobe").arg("zram").status();
    let dev = std::path::Path::new("/sys/block/zram0");
    if std::fs::read_to_string(dev.join("disksize"))?.trim() != "0" {
        return Ok(()); // set up by someone else
    }
    // lz4 where the kernel has it (berth's does), its default otherwise.
    let _ = std::fs::write(dev.join("comp_algorithm"), "lz4");
    let total = parse_meminfo(&std::fs::read_to_string("/proc/meminfo")?).total;
    std::fs::write(dev.join("disksize"), total.to_string())?;
    for (cmd, args) in [("mkswap", &["-q", "/dev/zram0"][..]), ("swapon", &["-p", "100", "/dev/zram0"][..])] {
        let out = std::process::Command::new(cmd).args(args).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!("{cmd}: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
    }
    Ok(())
}

/// Block on the PSI trigger forever, notifying the host on each event.
pub fn watch_pressure() {
    loop {
        if let Err(err) = watch_once() {
            eprintln!("berth-agent: memory pressure watch: {err}");
        }
        std::thread::sleep(Duration::from_secs(10));
    }
}

fn watch_once() -> io::Result<()> {
    let mut psi = std::fs::OpenOptions::new().read(true).write(true).open("/proc/pressure/memory")?;
    psi.write_all(PSI_TRIGGER)?;
    let mut last = None::<Instant>;
    loop {
        let mut pfd = libc::pollfd { fd: psi.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if pfd.revents & libc::POLLERR != 0 {
            return Err(io::Error::other("PSI trigger error"));
        }
        if pfd.revents & libc::POLLPRI != 0 && last.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
            last = Some(Instant::now());
            // Best effort; the host's poll loop catches up if this fails.
            if let Ok(fd) = vsock_connect(VMADDR_CID_HOST, HOST_VSOCK_PORT) {
                let mut host = std::os::unix::net::UnixStream::from(fd);
                let _ = host.write_all(b"memory-pressure\n");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo_and_psi() {
        let m = parse_meminfo("MemTotal:       16384000 kB\nMemFree:  100 kB\nMemAvailable:   8192000 kB\n");
        assert_eq!(m.total, 16384000 * 1024);
        assert_eq!(m.available, 8192000 * 1024);
        let psi = "some avg10=1.25 avg60=0.10 avg300=0.00 total=1234\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(parse_psi_some_avg10(psi), Some(1.25));
        assert_eq!(parse_balloon_pages("balloon_inflate 3689728\nballoon_deflate 2007296\nballoon_migrate 0\n"), 1682432);
    }
}
