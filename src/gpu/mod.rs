//! berth-gpu: the Linux VM's display, a virtio-gpu device (ID 16) on
//! macOS 27's custom Virtio API, so the guest uses its stock `virtio_gpu`
//! DRM driver (built into berth's kernel, guest/kernel/config.fragment).
//! Plan: docs/plans/gui-vms-agent-testing.md, "Linux display foundation".
//!
//! - protocol.rs: the virtio-gpu wire format;
//! - server.rs: the resource table and the 2D commands;
//! - frames.rs: the latest frame of each scanout, for screenshots and the
//!   app (`frames::latest`), and a PNG encoder;
//! - device.rs: the Virtualization.framework side (queues, guest memory,
//!   the shared memory region, snapshots);
//! - virgl.rs: where virglrenderer (3D through ANGLE on Metal) plugs in,
//!   behind the `virgl` cargo feature.
//!
//! Off unless ~/.berth/config.json has `"gui": true`: then `attach` adds
//! the device to the VM (macOS 27 only; not in safe mode). With the flag
//! off nothing here runs and the VM's configuration is exactly as before.
//! `"gpu_memory_mib"` (default 256) sizes the host-visible shared memory
//! region; `"gpu_display": "WxH"` (default 1920x1080) the display mode.

pub mod device;
pub mod frames;
pub mod protocol;
pub mod server;
pub mod virgl;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObject};
use objc2::msg_send;
use objc2_foundation::NSArray;

use crate::paths::Paths;

/// Default size of the host-visible shared memory region, in MiB.
pub const DEFAULT_MEMORY_MIB: u64 = 256;
const DEFAULT_DISPLAY: (u32, u32) = (1920, 1080);

/// The GPU settings from config.json. Read here until config.rs carries
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub gui: bool,
    pub memory_mib: u64,
    pub display: (u32, u32),
}

impl Settings {
    pub fn load(paths: &Paths) -> Self {
        Self::parse(&std::fs::read(paths.config()).unwrap_or_default())
    }

    fn parse(json: &[u8]) -> Self {
        #[derive(serde::Deserialize, Default)]
        struct Config {
            gui: Option<bool>,
            gpu_memory_mib: Option<u64>,
            gpu_display: Option<String>,
        }
        let c: Config = serde_json::from_slice(json).unwrap_or_default();
        Self {
            gui: c.gui.unwrap_or(false),
            memory_mib: c.gpu_memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            display: c.gpu_display.as_deref().and_then(parse_display).unwrap_or(DEFAULT_DISPLAY),
        }
    }

    /// The shared memory region's size in bytes. A PCI BAR is a power of
    /// two, so the setting is rounded up to one (16 MiB to 16 GiB) rather
    /// than leaving it to fail the VM's configuration.
    pub fn region_bytes(&self) -> u64 {
        self.memory_mib.clamp(16, 16 << 10).next_power_of_two() << 20
    }
}

/// "1920x1080" -> (1920, 1080); sizes from 640x480 to 7680x4320.
fn parse_display(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
    ((640..=7680).contains(&w) && (480..=4320).contains(&h)).then_some((w, h))
}

/// Add the GPU device to a VM configuration, after virtiofs::attach set
/// the other custom devices: it's appended to them, so their PCI slots
/// (and saved states made without the GPU) stay as they were.
pub fn attach(config: &objc2_virtualization::VZVirtualMachineConfiguration, paths: &Paths) {
    let settings = Settings::load(paths);
    if !settings.gui {
        return;
    }
    if AnyClass::get(c"VZCustomVirtioDeviceConfiguration").is_none() {
        eprintln!("gpu: \"gui\" is on, but the display needs macOS 27's custom Virtio devices");
        return;
    }
    if crate::vm::safe_boot(paths) {
        eprintln!("gpu: safe mode, no display device");
        return;
    }
    let device = device::configuration(&settings, paths);
    unsafe {
        let existing: Retained<NSArray<NSObject>> = msg_send![config, customVirtioDevices];
        let mut devices = existing.to_vec();
        devices.push(device);
        let devices = NSArray::from_retained_slice(&devices);
        let _: () = msg_send![config, setCustomVirtioDevices: &*devices];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_off() {
        assert_eq!(Settings::parse(b""), Settings { gui: false, memory_mib: 256, display: (1920, 1080) });
        assert!(!Settings::parse(b"{\"cpus\": 4}").gui);
        let s = Settings::parse(br#"{"gui": true, "gpu_memory_mib": 512, "gpu_display": "2560x1600"}"#);
        assert_eq!(s, Settings { gui: true, memory_mib: 512, display: (2560, 1600) });
        assert_eq!(Settings::parse(br#"{"gui": true, "gpu_display": "huge"}"#).display, (1920, 1080));
    }

    #[test]
    fn region_sizes_are_powers_of_two() {
        let at = |mib| Settings { gui: true, memory_mib: mib, display: DEFAULT_DISPLAY }.region_bytes();
        assert_eq!(at(256), 256 << 20);
        assert_eq!(at(300), 512 << 20);
        assert_eq!(at(0), 16 << 20);
        assert_eq!(at(1 << 40), 16 << 30);
    }

    #[test]
    fn display_sizes() {
        assert_eq!(parse_display("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_display(" 3840 X 2160 "), Some((3840, 2160)));
        assert_eq!(parse_display("100x100"), None);
        assert_eq!(parse_display("1920"), None);
    }
}
