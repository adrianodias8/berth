//! The GPU on macOS 27's custom Virtio API (VZCustomVirtioDeviceConfiguration):
//! device ID 16 with a control queue and a cursor queue, guest memory
//! through VZGuestMemoryMapping, and a host-visible shared memory region
//! (VZVirtioSharedMemoryRegionConfiguration, ID 1) for blob mappings. The
//! commands themselves are served by server.rs.
//!
//! The classes are new in macOS 27 and absent from objc2-virtualization;
//! virtiofs.rs declares the ones both devices use and this file the rest
//! (from the SDK's Virtualization.framework headers).
//!
//! Offered features: EDID and RESOURCE_UUID. VIRGL, CONTEXT_INIT and
//! RESOURCE_BLOB wait for the 3D path (virgl.rs): offering BLOB alone would
//! make Linux allocate its dumb framebuffers as guest blobs, which the 2D
//! server doesn't take.
//!
//! **Config-space writes (spike S6).** The framework gives a custom device
//! no callback when the guest writes its configuration space, and the
//! guest acknowledges display events by writing `events_clear`. If we ever
//! set `events_read` (through updateDeviceSpecificConfiguration), we'd
//! never see that acknowledgement, and the bit would stay set: Linux's
//! driver re-reads display info on every later config interrupt and can't
//! tell a new event from the old one. So `events_read` is always 0, and a
//! display size change takes a device reset or a reboot until S6 shows
//! another way (for example clearing it ourselves once the guest's
//! GET_DISPLAY_INFO arrives).
//!
//! Debugging (spike S1): while `~/.berth/gpu/dump-frames` exists, scanout
//! 0's latest frame is written to `~/.berth/gpu/scanout-0.png`, at most once
//! a second.

#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dispatch2::{DispatchQoS, DispatchQueue, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, extern_class, extern_methods, msg_send};
use objc2_foundation::{NSArray, NSData};

use super::frames::DISPLAY;
use super::protocol::{self, CtrlHdr, DeviceConfig, Response, cmd};
use super::server::{GuestMemory, Mapping, Server};
use super::{Settings, virgl};
use crate::virtiofs::{
    VZCustomVirtioDevice, VZCustomVirtioDeviceConfiguration, VZCustomVirtioDeviceConfigurationDelegate,
    VZCustomVirtioDeviceDelegate, VZCustomVirtioDeviceDelegateProvider, VZVirtioDeviceSpecificConfiguration, VZVirtioQueue,
};

/// controlq and cursorq.
const QUEUES: u16 = 2;
/// Bytes all 2D resources together may take on the Mac: a few 4K
/// framebuffers and cursors fit many times over.
const HOST_PIXEL_LIMIT: u64 = 1 << 30;
/// Largest request we read: an attach with the most entries we accept.
const MAX_REQUEST: usize = protocol::MAX_ENTRIES as usize * 16 + 64;

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZGuestMemoryMapping;
);
impl VZGuestMemoryMapping {
    extern_methods!(
        #[unsafe(method(mutableBytes))]
        #[unsafe(method_family = none)]
        pub unsafe fn mutableBytes(&self) -> *mut std::ffi::c_void;
        #[unsafe(method(length))]
        #[unsafe(method_family = none)]
        pub unsafe fn length(&self) -> usize;
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZVirtioFeatureSet;
);
impl VZVirtioFeatureSet {
    extern_methods!(
        #[unsafe(method(subset0))]
        #[unsafe(method_family = none)]
        pub unsafe fn subset0(&self) -> u32;
        #[unsafe(method(setSubset0:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setSubset0(&self, bits: u32);
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZNegotiatedVirtioFeatureSet;
);
impl VZNegotiatedVirtioFeatureSet {
    extern_methods!(
        #[unsafe(method(subset0))]
        #[unsafe(method_family = none)]
        pub unsafe fn subset0(&self) -> u32;
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZVirtioSharedMemoryRegionConfiguration;
);
impl VZVirtioSharedMemoryRegionConfiguration {
    extern_methods!(
        #[unsafe(method(initWithRegionID:size:))]
        #[unsafe(method_family = init)]
        pub unsafe fn initWithRegionID_size(this: objc2::rc::Allocated<Self>, id: u8, size: u64) -> Retained<Self>;
    );
}

/// What virtiofs.rs doesn't declare of the configuration and the device.
impl VZCustomVirtioDeviceConfiguration {
    extern_methods!(
        #[unsafe(method(optionalFeatures))]
        #[unsafe(method_family = none)]
        pub unsafe fn optionalFeatures(&self) -> Retained<VZVirtioFeatureSet>;
        #[unsafe(method(setSharedMemoryRegions:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setSharedMemoryRegions(&self, regions: &NSArray<VZVirtioSharedMemoryRegionConfiguration>);
        #[unsafe(method(maximumAllowedSharedMemoryRegionCount))]
        #[unsafe(method_family = none)]
        pub unsafe fn maximumAllowedSharedMemoryRegionCount() -> usize;
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZVirtioSharedMemoryRegion;
);
impl VZVirtioSharedMemoryRegion {
    extern_methods!(
        #[unsafe(method(regionID))]
        #[unsafe(method_family = none)]
        pub unsafe fn regionID(&self) -> u8;
        #[unsafe(method(size))]
        #[unsafe(method_family = none)]
        pub unsafe fn size(&self) -> u64;
        #[unsafe(method(mapMemory:atOffset:size:completionHandler:))]
        #[unsafe(method_family = none)]
        pub unsafe fn mapMemory_atOffset_size_completionHandler(
            &self,
            memory: *mut std::ffi::c_void,
            offset: u64,
            size: u64,
            handler: &block2::DynBlock<dyn Fn(*mut objc2_foundation::NSError)>,
        );
    );
}

impl VZCustomVirtioDevice {
    extern_methods!(
        #[unsafe(method(sharedMemoryRegions))]
        #[unsafe(method_family = none)]
        pub unsafe fn sharedMemoryRegions(&self) -> Retained<NSArray<VZVirtioSharedMemoryRegion>>;
        #[unsafe(method(guestMemoryMappingAtPhysicalAddress:length:))]
        #[unsafe(method_family = none)]
        pub unsafe fn guestMemoryMappingAtPhysicalAddress_length(
            &self,
            address: u64,
            length: usize,
        ) -> Option<Retained<VZGuestMemoryMapping>>;
        #[unsafe(method(negotiatedFeatures))]
        #[unsafe(method_family = none)]
        pub unsafe fn negotiatedFeatures(&self) -> Option<Retained<VZNegotiatedVirtioFeatureSet>>;
    );
}

/// Guest RAM through the device.
struct VzMemory<'a>(&'a VZCustomVirtioDevice);

struct VzMapping {
    _mapping: Retained<VZGuestMemoryMapping>,
    ptr: *const u8,
    len: usize,
}

impl Mapping for VzMapping {
    fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
    fn len(&self) -> usize {
        self.len
    }
}

impl GuestMemory for VzMemory<'_> {
    fn map(&self, addr: u64, len: usize) -> Option<Box<dyn Mapping>> {
        // The mapping stays valid while we hold it: its object keeps the
        // VM's memory mapped into our address space.
        let mapping = unsafe { self.0.guestMemoryMappingAtPhysicalAddress_length(addr, len) }?;
        let (ptr, mapped) = unsafe { (mapping.mutableBytes() as *const u8, mapping.length()) };
        if ptr.is_null() || mapped < len {
            return None;
        }
        Some(Box::new(VzMapping { _mapping: mapping, ptr, len: mapped }))
    }
}

/// Writes scanout 0 to a PNG while the marker file exists (spike S1).
struct Dump {
    dir: PathBuf,
    checked: Cell<Option<Instant>>,
    written: Cell<u64>,
}

impl Dump {
    /// Called after each batch of requests; costs a clock read unless a
    /// second has passed.
    fn tick(&self) {
        if self.checked.get().is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.checked.set(Some(Instant::now()));
        if !self.dir.join("dump-frames").exists() {
            return;
        }
        let Some(frame) = DISPLAY.latest(0) else { return };
        if frame.sequence == self.written.get() {
            return;
        }
        self.written.set(frame.sequence);
        let dir = self.dir.clone();
        // Encoding a 1080p frame takes milliseconds: not on the device queue.
        std::thread::spawn(move || {
            let tmp = dir.join(".scanout-0.png.tmp");
            if std::fs::write(&tmp, frame.png()).is_ok() {
                let _ = std::fs::rename(&tmp, dir.join("scanout-0.png"));
            }
        });
    }
}

pub struct Ivars {
    /// Device queue only, like every call the framework makes.
    server: RefCell<Server>,
    dump: Dump,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this type has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "BerthVirtioGpuDevice"]
    #[ivars = Ivars]
    pub struct GpuDevice;

    unsafe impl NSObjectProtocol for GpuDevice {}

    unsafe impl VZCustomVirtioDeviceConfigurationDelegate for GpuDevice {
        #[unsafe(method(customVirtioConfiguration:didCreateDevice:))]
        fn did_create(&self, _configuration: &VZCustomVirtioDeviceConfiguration, device: &VZCustomVirtioDevice) {
            unsafe { device.setDelegate(Some(ProtocolObject::from_ref(self))) };
            eprintln!("gpu: device created");
        }
    }

    unsafe impl VZCustomVirtioDeviceDelegate for GpuDevice {
        #[unsafe(method(customVirtioDevice:didReceiveNotificationForQueue:))]
        fn did_notify(&self, device: &VZCustomVirtioDevice, queue: &VZVirtioQueue) {
            self.drain(device, queue);
        }

        #[unsafe(method(customVirtioDeviceDidAcceptDriverOk:))]
        fn driver_ok(&self, device: &VZCustomVirtioDevice) {
            let features = unsafe { device.negotiatedFeatures() }.map(|f| unsafe { f.subset0() });
            eprintln!("gpu: driver ready, features {}", features.map_or("unknown".into(), describe_features));
            if self.ivars().dump.dir.join("spike-s4").exists() {
                spike_s4(device);
            }
        }

        #[unsafe(method(customVirtioDeviceWillReset:))]
        fn will_reset(&self, _device: &VZCustomVirtioDevice) {
            self.ivars().server.borrow_mut().reset();
        }

        /// For a VM snapshot (snapshots.rs) or `berth stop`'s saved state:
        /// requests are served synchronously, so none is in flight.
        #[unsafe(method_id(customVirtioDeviceSaveStateForRestore:))]
        fn save_state(&self, _device: &VZCustomVirtioDevice) -> Option<Retained<NSData>> {
            Some(NSData::with_bytes(&self.ivars().server.borrow().save_state()))
        }

        #[unsafe(method(customVirtioDeviceShouldRestore:saveState:))]
        fn restore_state(&self, device: &VZCustomVirtioDevice, save_state: &NSData) -> bool {
            let ok = self.ivars().server.borrow_mut().restore_state(&save_state.to_vec(), &VzMemory(device));
            eprintln!("gpu: {} the display's state from the snapshot", if ok { "restored" } else { "couldn't restore" });
            ok
        }
    }
);

impl GpuDevice {
    fn new(server: Server, dump_dir: PathBuf) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            server: RefCell::new(server),
            dump: Dump { dir: dump_dir, checked: Cell::new(None), written: Cell::new(0) },
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Serve everything on `queue`. Both queues carry ordinary requests;
    /// the cursor queue's have no room for a reply, so none is written.
    /// `nextElement` keeps notifications off until the queue is empty, so
    /// this always drains it.
    fn drain(&self, device: &VZCustomVirtioDevice, queue: &VZVirtioQueue) {
        let memory = VzMemory(device);
        while let Some(element) = unsafe { queue.nextElement() } {
            let len = unsafe { element.readBuffersByteCount() };
            let mut reply = if len > MAX_REQUEST {
                Response::Err(cmd::RESP_ERR_UNSPEC).serialize(&CtrlHdr::default())
            } else {
                let mut request = vec![0u8; len];
                match unsafe { element.readBytesIntoBuffer_exactLength_error(request.as_mut_ptr().cast(), len) } {
                    Ok(()) => self.ivars().server.borrow_mut().handle(&request, &memory),
                    Err(_) => Response::Err(cmd::RESP_ERR_UNSPEC).serialize(&CtrlHdr::default()),
                }
            };
            let room = unsafe { element.writeBuffersByteCount() };
            if reply.len() > room && room >= CtrlHdr::SIZE {
                // The guest reads whatever header is in its buffer: say it
                // failed rather than leave it looking like success.
                let hdr = CtrlHdr::parse(&reply).unwrap_or_default();
                reply = Response::Err(cmd::RESP_ERR_UNSPEC).serialize(&hdr);
            }
            if reply.len() <= room {
                let _ = unsafe { element.writeBuffer_exactLength_error(reply.as_mut_ptr().cast(), reply.len()) };
            }
            unsafe { element.returnToQueue() };
        }
        self.ivars().dump.tick();
    }
}

/// Spike S4 (bench/spikes/gpu): put host memory into the guest's view
/// through the shared memory region, as blob mappings and Venus will.
/// Page-aligned anonymous memory filled with a pattern stands in for an
/// MTLBuffer's contents (newBufferWithBytesNoCopy takes exactly that); the
/// guest reads it back through the region's PCI BAR. Only while
/// `~/.berth/gpu/spike-s4` exists; the memory is never freed (a spike).
fn spike_s4(device: &VZCustomVirtioDevice) {
    const SIZE: usize = 2 << 20;
    let regions = unsafe { device.sharedMemoryRegions() };
    let Some(region) = regions.iter().find(|r| unsafe { r.regionID() } == protocol::SHM_ID_HOST_VISIBLE) else {
        eprintln!("gpu: S4: the device has no host-visible region");
        return;
    };
    let memory = unsafe {
        libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0)
    };
    if memory == libc::MAP_FAILED {
        eprintln!("gpu: S4: mmap: {}", std::io::Error::last_os_error());
        return;
    }
    let pattern = b"berth-s4";
    let bytes = unsafe { std::slice::from_raw_parts_mut(memory as *mut u8, SIZE) };
    for chunk in bytes.chunks_mut(pattern.len()) {
        chunk.copy_from_slice(&pattern[..chunk.len()]);
    }
    let started = Instant::now();
    let size = unsafe { region.size() };
    let handler = block2::RcBlock::new(move |err: *mut objc2_foundation::NSError| {
        match unsafe { err.as_ref() } {
            None => eprintln!("gpu: S4: mapped {} MiB at offset 0 of the {} MiB region in {:?}", SIZE >> 20, size >> 20, started.elapsed()),
            Some(e) => eprintln!("gpu: S4: mapMemory failed: {}", e.localizedDescription()),
        }
    });
    unsafe { region.mapMemory_atOffset_size_completionHandler(memory, 0, SIZE as u64, &handler) };
}

fn describe_features(bits: u32) -> String {
    let names = [
        (protocol::F_VIRGL, "virgl"),
        (protocol::F_EDID, "edid"),
        (protocol::F_RESOURCE_UUID, "resource-uuid"),
        (protocol::F_RESOURCE_BLOB, "resource-blob"),
        (protocol::F_CONTEXT_INIT, "context-init"),
    ];
    let on: Vec<_> = names.iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| *n).collect();
    if on.is_empty() { "none".into() } else { on.join(", ") }
}

/// Keeps the delegate alive for the VM's lifetime (the framework holds it
/// weakly).
struct Keep(#[allow(dead_code)] Retained<GpuDevice>);
unsafe impl Send for Keep {}
static KEEP: Mutex<Vec<Keep>> = Mutex::new(Vec::new());

/// The device's configuration, for VZVirtualMachineConfiguration's
/// customVirtioDevices.
pub fn configuration(settings: &Settings, paths: &crate::paths::Paths) -> Retained<NSObject> {
    let capsets = virgl::capsets();
    let server = Server::new(vec![settings.display], capsets, HOST_PIXEL_LIMIT, &DISPLAY);
    let space = DeviceConfig { events_read: 0, events_clear: 0, num_scanouts: server.num_scanouts(), num_capsets: server.num_capsets() };
    let mut features = protocol::F_EDID | protocol::F_RESOURCE_UUID;
    if server.num_capsets() > 0 {
        features |= protocol::F_VIRGL;
    }
    // Requests are short and the guest's compositor waits on each one.
    let queue = DispatchQueue::new_with_target(
        "dev.berth.gpu",
        None,
        Some(&DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::UserInteractive))),
    );
    let device = GpuDevice::new(server, paths.root.join("gpu"));
    unsafe {
        let specific = VZVirtioDeviceSpecificConfiguration::initWithConfigurationData(
            VZVirtioDeviceSpecificConfiguration::alloc(),
            &NSData::with_bytes(&space.to_bytes()),
        );
        let provider = VZCustomVirtioDeviceDelegateProvider::initWithDeviceQueue_delegate(
            VZCustomVirtioDeviceDelegateProvider::alloc(),
            &queue,
            ProtocolObject::from_ref(&*device),
        );
        let config = VZCustomVirtioDeviceConfiguration::new();
        config.setDeviceID(protocol::VIRTIO_ID_GPU);
        config.setPCIClassID(0x03); // display controller
        config.setPCISubclassID(0x80); // other (not VGA-compatible)
        config.setVirtioQueueCount(QUEUES);
        config.setDeviceSpecificConfiguration(Some(&specific));
        let optional = config.optionalFeatures();
        optional.setSubset0(optional.subset0() | features);
        // The host-visible region blob resources map into (Venus needs
        // it); declared from the start so guests see the same device later.
        let region = settings.region_bytes();
        if VZCustomVirtioDeviceConfiguration::maximumAllowedSharedMemoryRegionCount() >= 1 {
            let shm = VZVirtioSharedMemoryRegionConfiguration::initWithRegionID_size(
                VZVirtioSharedMemoryRegionConfiguration::alloc(),
                protocol::SHM_ID_HOST_VISIBLE,
                region,
            );
            config.setSharedMemoryRegions(&NSArray::from_retained_slice(&[shm]));
        }
        config.setProvider(Some(&provider));
        config.setSupportsSaveRestore(true);
        eprintln!(
            "gpu: virtio-gpu {}x{}, features {}, {} MiB shared memory region",
            settings.display.0,
            settings.display.1,
            describe_features(features),
            region >> 20
        );
        KEEP.lock().unwrap().push(Keep(device));
        Retained::into_super(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_names() {
        assert_eq!(describe_features(0), "none");
        assert_eq!(describe_features(protocol::F_EDID | protocol::F_RESOURCE_UUID), "edid, resource-uuid");
    }

    /// The classes and selectors declared here exist on this Mac (macOS
    /// 27): a typo in a selector would otherwise only show in a VM.
    #[test]
    fn declared_selectors_exist() {
        use objc2::runtime::{AnyClass, Sel};
        let Some(config) = AnyClass::get(c"VZCustomVirtioDeviceConfiguration") else { return };
        let has = |class: &AnyClass, sel: &str| class.instance_method(Sel::register(&std::ffi::CString::new(sel).unwrap())).is_some();
        for sel in ["optionalFeatures", "setSharedMemoryRegions:", "setDeviceID:", "setVirtioQueueCount:"] {
            assert!(has(config, sel), "{sel}");
        }
        assert!(config.class_method(objc2::sel!(maximumAllowedSharedMemoryRegionCount)).is_some());
        let device = AnyClass::get(c"VZCustomVirtioDevice").unwrap();
        assert!(has(device, "guestMemoryMappingAtPhysicalAddress:length:"));
        assert!(has(device, "negotiatedFeatures") && has(device, "sharedMemoryRegions"));
        let region = AnyClass::get(c"VZVirtioSharedMemoryRegion").unwrap();
        for sel in ["regionID", "size", "mapMemory:atOffset:size:completionHandler:"] {
            assert!(has(region, sel), "{sel}");
        }
        let mapping = AnyClass::get(c"VZGuestMemoryMapping").unwrap();
        assert!(has(mapping, "mutableBytes") && has(mapping, "length"));
        // Our delegate registered every callback under the protocol's names.
        use objc2::ClassType;
        for sel in [
            "customVirtioConfiguration:didCreateDevice:",
            "customVirtioDevice:didReceiveNotificationForQueue:",
            "customVirtioDeviceDidAcceptDriverOk:",
            "customVirtioDeviceWillReset:",
            "customVirtioDeviceSaveStateForRestore:",
            "customVirtioDeviceShouldRestore:saveState:",
        ] {
            assert!(has(GpuDevice::class(), sel), "{sel}");
        }
        let shm = AnyClass::get(c"VZVirtioSharedMemoryRegionConfiguration").unwrap();
        assert!(has(shm, "initWithRegionID:size:"));
        let vm = AnyClass::get(c"VZVirtualMachineConfiguration").unwrap();
        assert!(has(vm, "customVirtioDevices") && has(vm, "setCustomVirtioDevices:"));
        let set = AnyClass::get(c"VZVirtioFeatureSet").unwrap();
        assert!(has(set, "subset0") && has(set, "setSubset0:"));
    }

    /// The whole device configuration builds with the framework's classes
    /// (no VM is created or started).
    #[test]
    fn configuration_builds() {
        if objc2::runtime::AnyClass::get(c"VZCustomVirtioDeviceConfiguration").is_none() {
            return;
        }
        let settings = Settings { gui: true, memory_mib: 256, display: (1280, 800) };
        let paths = crate::paths::Paths { home: std::env::temp_dir(), root: std::env::temp_dir().join("berth-gpu-test") };
        let device = configuration(&settings, &paths);
        let config: Retained<VZCustomVirtioDeviceConfiguration> = device.downcast().unwrap();
        unsafe {
            assert_eq!(config.optionalFeatures().subset0() & protocol::F_EDID, protocol::F_EDID);
            let regions: Retained<NSArray<NSObject>> = msg_send![&*config, sharedMemoryRegions];
            assert_eq!(regions.len(), 1);
            let size: u64 = msg_send![&*regions.objectAtIndex(0), size];
            assert_eq!(size, 256 << 20);
            let id: u16 = msg_send![&*config, deviceID];
            assert_eq!(id, 16);
        }
    }

    /// The framework accepts it in a VM configuration: validation checks
    /// the queue count, region size and IDs (no VM is created or started).
    #[test]
    fn configuration_validates() {
        use objc2_virtualization::*;
        if objc2::runtime::AnyClass::get(c"VZCustomVirtioDeviceConfiguration").is_none() {
            return;
        }
        let settings = Settings { gui: true, memory_mib: 300, display: (1920, 1080) };
        let paths = crate::paths::Paths { home: std::env::temp_dir(), root: std::env::temp_dir().join("berth-gpu-test") };
        let device = configuration(&settings, &paths);
        unsafe {
            let config = VZVirtualMachineConfiguration::new();
            config.setCPUCount(2);
            config.setMemorySize(1 << 30);
            config.setPlatform(&VZGenericPlatformConfiguration::new());
            let kernel = objc2_foundation::NSURL::fileURLWithPath(&objc2_foundation::NSString::from_str("/dev/null"));
            let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel);
            config.setBootLoader(Some(&boot));
            let devices = NSArray::from_retained_slice(&[device]);
            let _: () = msg_send![&*config, setCustomVirtioDevices: &*devices];
            if let Err(e) = config.validateWithError() {
                panic!("{}", e.localizedDescription());
            }
        }
    }
}
