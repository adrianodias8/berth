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
//! Offered features: EDID and RESOURCE_UUID; VIRGL when the 3D renderer
//! started (virgl.rs); CONTEXT_INIT and RESOURCE_BLOB only with `gpu_blob`
//! too (`features`).
//!
//! **Requests that wait.** A fenced request is answered when the renderer
//! signals its fence: `drain` still empties the queue (notifications stay
//! off until it does) but keeps such elements in `pending`, and the
//! renderer's signals, from its berth-virgl thread, are posted back onto
//! the device queue, where they're answered. MAP_BLOB / UNMAP_BLOB wait the
//! same way for the shared memory region's completion. virtiofs.rs answers
//! elements after `drain` returns too.
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

use dispatch2::{DispatchQoS, DispatchQueue, DispatchRetained, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, Message, define_class, extern_class, extern_methods, msg_send};
use objc2_foundation::{NSArray, NSData};

use super::Settings;
use super::backend::Backend3d;
use super::fence::{Pending, Signal};
use super::frames::DISPLAY;
use super::protocol::{self, CtrlHdr, DeviceConfig, Rect, Response, cmd};
use super::server::{Deferred, GuestMemory, Mapping, Reply, Server};
use crate::virtiofs::{
    VZCustomVirtioDevice, VZCustomVirtioDeviceConfiguration, VZCustomVirtioDeviceConfigurationDelegate,
    VZCustomVirtioDeviceDelegate, VZCustomVirtioDeviceDelegateProvider, VZVirtioDeviceSpecificConfiguration, VZVirtioQueue,
    VZVirtioQueueElement,
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
        #[unsafe(method(unmapMemoryAtOffset:size:completionHandler:))]
        #[unsafe(method_family = none)]
        pub unsafe fn unmapMemoryAtOffset_size_completionHandler(
            &self,
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

/// A request off a virtqueue, as serving it needs: generic, so the path a
/// fenced or deferred request takes is tested with fake elements.
trait Element {
    /// The request's bytes; None if it's larger than `max` or unreadable.
    fn request(&self, max: usize) -> Option<Vec<u8>>;
    /// Room for the reply.
    fn room(&self) -> usize;
    fn write(&self, reply: &mut [u8]);
    /// Hand it back to the guest, which then reads the reply.
    fn give_back(&self);
}

impl Element for Retained<VZVirtioQueueElement> {
    fn request(&self, max: usize) -> Option<Vec<u8>> {
        let len = unsafe { self.readBuffersByteCount() };
        if len > max {
            return None;
        }
        let mut request = vec![0u8; len];
        unsafe { self.readBytesIntoBuffer_exactLength_error(request.as_mut_ptr().cast(), len) }.ok()?;
        Some(request)
    }
    fn room(&self) -> usize {
        unsafe { self.writeBuffersByteCount() }
    }
    fn write(&self, reply: &mut [u8]) {
        let _ = unsafe { self.writeBuffer_exactLength_error(reply.as_mut_ptr().cast(), reply.len()) };
    }
    fn give_back(&self) {
        unsafe { self.returnToQueue() };
    }
}

/// Write `reply` (when there's room: the cursor queue's requests have none)
/// and return the element.
fn answer<E: Element>(element: &E, mut reply: Vec<u8>) {
    let room = element.room();
    if reply.len() > room && room >= CtrlHdr::SIZE {
        // The guest reads whatever header is in its buffer: say it
        // failed rather than leave it looking like success.
        let hdr = CtrlHdr::parse(&reply).unwrap_or_default();
        reply = Response::Err(cmd::RESP_ERR_UNSPEC).serialize(&hdr);
    }
    if reply.len() <= room {
        element.write(&mut reply);
    }
    element.give_back();
}

/// The same reply, as an error (it keeps the fence the guest waits on).
fn failed(reply: &[u8]) -> Vec<u8> {
    Response::Err(cmd::RESP_ERR_UNSPEC).serialize(&CtrlHdr::parse(reply).unwrap_or_default())
}

/// Serve one element: answered now, held in `pending` until its fence
/// signals, or handed back with the shared memory work it waits on.
fn serve<E: Element>(server: &mut Server, mem: &dyn GuestMemory, pending: &mut Pending<E>, element: E) -> Option<(E, Deferred, Vec<u8>)> {
    let Some(request) = element.request(MAX_REQUEST) else {
        answer(&element, Response::Err(cmd::RESP_ERR_UNSPEC).serialize(&CtrlHdr::default()));
        return None;
    };
    let reply = server.handle(&request, mem);
    // Requests fenced on a context's rings that CTX_DESTROY took away:
    // the renderer forgets those fences, so answer them now rather than
    // never (the guest's driver would wait on them, and saves would fail).
    for ctx in server.take_destroyed() {
        for (waiting, bytes) in pending.take_context(ctx) {
            answer(&waiting, bytes);
        }
    }
    match reply {
        Reply::Now(reply) => answer(&element, reply),
        Reply::Fenced { on, bytes } => pending.push(on, element, bytes),
        Reply::Deferred { op, bytes } => return Some((element, op, bytes)),
    }
    None
}

/// Answer what `signals` release.
fn release<E: Element>(pending: &mut Pending<E>, signals: &[Signal]) {
    for (element, reply) in pending.complete(signals) {
        answer(&element, reply);
    }
}

/// Answer everything still waiting with an error (a device reset).
fn fail_all<E: Element>(pending: &mut Pending<E>) {
    for (element, reply) in pending.take_all() {
        answer(&element, failed(&reply));
    }
}

/// The device carried to its own queue from another thread (the renderer's
/// fence signals, read_resource). Only ever used on the device queue.
struct OnQueue(Retained<GpuDevice>);
unsafe impl Send for OnQueue {}

pub struct Ivars {
    /// Device queue only, like every call the framework makes.
    server: RefCell<Server>,
    /// Fenced requests waiting for the renderer (device queue only).
    pending: RefCell<Pending<Retained<VZVirtioQueueElement>>>,
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
            self.ivars().server.borrow_mut().set_negotiated(features.unwrap_or(0));
            eprintln!("gpu: driver ready, features {}", features.map_or("unknown".into(), describe_features));
            if self.ivars().dump.dir.join("spike-s4").exists() {
                spike_s4(device);
            }
        }

        /// Requests still waiting for a fence are answered with an error,
        /// then everything goes; blobs still in the region are taken out.
        #[unsafe(method(customVirtioDeviceWillReset:))]
        fn will_reset(&self, device: &VZCustomVirtioDevice) {
            fail_all(&mut self.ivars().pending.borrow_mut());
            let unmaps = self.ivars().server.borrow_mut().reset();
            for op in unmaps {
                self.region_work(device, op, None);
            }
        }

        /// For a VM snapshot (snapshots.rs) or `berth stop`'s saved state.
        /// 2D requests are served synchronously, so none is in flight; what
        /// lives in the renderer can't be saved, so while there's any the
        /// save fails and berth falls back to the disk.
        #[unsafe(method_id(customVirtioDeviceSaveStateForRestore:))]
        fn save_state(&self, _device: &VZCustomVirtioDevice) -> Option<Retained<NSData>> {
            let state = if self.ivars().pending.borrow().is_empty() {
                self.ivars().server.borrow().save_state()
            } else {
                Err("fenced requests in flight")
            };
            match state {
                Ok(state) => Some(NSData::with_bytes(&state)),
                Err(why) => {
                    eprintln!("gpu: can't save the display's state: {why}");
                    None
                }
            }
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
            pending: RefCell::new(Pending::default()),
            dump: Dump { dir: dump_dir, checked: Cell::new(None), written: Cell::new(0) },
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Serve everything on `queue`. Both queues carry ordinary requests;
    /// the cursor queue's have no room for a reply, so none is written.
    /// `nextElement` keeps notifications off until the queue is empty, so
    /// this always drains it; fenced requests stay behind in `pending`.
    fn drain(&self, device: &VZCustomVirtioDevice, queue: &VZVirtioQueue) {
        let memory = VzMemory(device);
        while let Some(element) = unsafe { queue.nextElement() } {
            let deferred = serve(&mut self.ivars().server.borrow_mut(), &memory, &mut self.ivars().pending.borrow_mut(), element);
            if let Some((element, op, reply)) = deferred {
                self.region_work(device, op, Some((element, reply)));
            }
        }
        self.ivars().dump.tick();
    }

    /// Fences the renderer signalled, on the device queue.
    fn signalled(&self, signals: &[Signal]) {
        release(&mut self.ivars().pending.borrow_mut(), signals);
    }

    /// Map or unmap a blob in the shared memory region, then answer
    /// `waiting` (if a request waits) and tell the server. The framework
    /// calls the completion on the device queue.
    fn region_work(&self, device: &VZCustomVirtioDevice, op: Deferred, waiting: Option<(Retained<VZVirtioQueueElement>, Vec<u8>)>) {
        let this = self.retain();
        let waiting = Cell::new(waiting);
        let regions = unsafe { device.sharedMemoryRegions() };
        let region = regions.iter().find(|r| unsafe { r.regionID() } == protocol::SHM_ID_HOST_VISIBLE);
        let finish = move |ok: bool| {
            let mut server = this.ivars().server.borrow_mut();
            match op {
                Deferred::Map { resource_id, .. } if !ok => server.map_failed(resource_id),
                Deferred::Unmap { resource_id, .. } if ok => server.unmapped(resource_id),
                _ => {}
            }
            if let Some((element, reply)) = waiting.take() {
                answer(&element, if ok { reply } else { failed(&reply) });
            }
        };
        let Some(region) = region else {
            eprintln!("gpu: no shared memory region for blob mappings");
            return finish(false);
        };
        let handler = block2::RcBlock::new(move |err: *mut objc2_foundation::NSError| {
            if let Some(e) = unsafe { err.as_ref() } {
                eprintln!("gpu: blob {op:?}: {}", e.localizedDescription());
            }
            finish(err.is_null());
        });
        unsafe {
            match op {
                Deferred::Map { offset, ptr, len, .. } => region.mapMemory_atOffset_size_completionHandler(ptr.cast(), offset, len, &handler),
                Deferred::Unmap { offset, len, .. } => region.unmapMemoryAtOffset_size_completionHandler(offset, len, &handler),
            }
        }
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

/// The feature bits a device with this server offers: VIRGL with a
/// renderer that has capsets, CONTEXT_INIT and RESOURCE_BLOB only with
/// blobs on (gpu_blob; offering BLOB without a renderer would make Linux
/// allocate its dumb framebuffers as guest blobs).
fn features(server: &Server) -> u32 {
    let mut features = protocol::F_EDID | protocol::F_RESOURCE_UUID;
    if server.has_backend() && server.num_capsets() > 0 {
        features |= protocol::F_VIRGL;
    }
    if server.blobs() {
        features |= protocol::F_CONTEXT_INIT | protocol::F_RESOURCE_BLOB;
    }
    features
}

/// Keeps the delegate alive for the VM's lifetime (the framework holds it
/// weakly).
struct Keep(#[allow(dead_code)] Retained<GpuDevice>);
unsafe impl Send for Keep {}
static KEEP: Mutex<Vec<Keep>> = Mutex::new(Vec::new());

/// The VM's device, for `read_resource` (one VM per process).
static CURRENT: Mutex<Option<(DispatchRetained<DispatchQueue>, OnQueue)>> = Mutex::new(None);

/// Read `rect` of resource `id` as BGRA into `dst` (rows `stride` apart),
/// on the device queue: for the display's stream of resources (frames.rs).
/// Never from the device queue itself. The daemon sets it as the display's
/// RESOURCE_READER.
pub fn read_resource(id: u32, rect: Rect, dst: &mut [u8], stride: usize) -> Result<(), String> {
    let (queue, device) = {
        let current = CURRENT.lock().unwrap();
        let (queue, device) = current.as_ref().ok_or("no GPU device")?;
        (queue.clone(), OnQueue(device.0.clone()))
    };
    let mut result = Err(cmd::RESP_ERR_UNSPEC);
    queue.exec_sync(|| {
        let device = device;
        result = device.0.ivars().server.borrow_mut().read_resource(id, rect, dst, stride);
    });
    result.map_err(|code| format!("resource {id}: error {code:#x}"))
}

/// The device's configuration, for VZVirtualMachineConfiguration's
/// customVirtioDevices. `backend` is the 3D renderer, if one started.
pub fn configuration(settings: &Settings, paths: &crate::paths::Paths, backend: Option<Box<dyn Backend3d>>) -> Retained<NSObject> {
    let region = settings.region_bytes();
    let has_region = unsafe { VZCustomVirtioDeviceConfiguration::maximumAllowedSharedMemoryRegionCount() } >= 1;
    let mut server = Server::new(vec![settings.display], vec![], HOST_PIXEL_LIMIT, &DISPLAY);
    if let Some(backend) = backend {
        server = server.with_backend(backend);
        if settings.blob && has_region {
            server = server.with_blobs(region);
        }
    }
    let space = DeviceConfig { events_read: 0, events_clear: 0, num_scanouts: server.num_scanouts(), num_capsets: server.num_capsets() };
    let features = features(&server);
    // Requests are short and the guest's compositor waits on each one.
    let queue = DispatchQueue::new_with_target(
        "dev.berth.gpu",
        None,
        Some(&DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::UserInteractive))),
    );
    let device = GpuDevice::new(server, paths.root.join("gpu"));
    // Fence signals come from the renderer's thread: answered on the queue.
    let (on_queue, signal_queue) = (OnQueue(device.clone()), queue.clone());
    device.ivars().server.borrow_mut().set_signal_sink(Box::new(move |signals| {
        let this = &on_queue;
        let device = OnQueue(this.0.clone());
        signal_queue.exec_async(move || {
            let device = device;
            device.0.signalled(&signals);
        });
    }));
    *CURRENT.lock().unwrap() = Some((queue.clone(), OnQueue(device.clone())));
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
        if has_region {
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
        for sel in ["regionID", "size", "mapMemory:atOffset:size:completionHandler:", "unmapMemoryAtOffset:size:completionHandler:"] {
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
        let settings = Settings { gui: true, memory_mib: 256, display: (1280, 800), virgl: false, blob: false };
        let paths = crate::paths::Paths { home: std::env::temp_dir(), root: std::env::temp_dir().join("berth-gpu-test") };
        let device = configuration(&settings, &paths, None);
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
        let settings = Settings { gui: true, memory_mib: 300, display: (1920, 1080), virgl: false, blob: false };
        let paths = crate::paths::Paths { home: std::env::temp_dir(), root: std::env::temp_dir().join("berth-gpu-test") };
        let device = configuration(&settings, &paths, None);
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

    /// Each fake element's id and reply, as they're handed back.
    type Log = std::rc::Rc<RefCell<Vec<(u32, Vec<u8>)>>>;

    /// Fake virtqueue elements: what each was answered, in order.
    struct Fake {
        id: u32,
        request: Vec<u8>,
        room: usize,
        log: Log,
        reply: RefCell<Vec<u8>>,
    }

    impl Element for Fake {
        fn request(&self, max: usize) -> Option<Vec<u8>> {
            (self.request.len() <= max).then(|| self.request.clone())
        }
        fn room(&self) -> usize {
            self.room
        }
        fn write(&self, reply: &mut [u8]) {
            *self.reply.borrow_mut() = reply.to_vec();
        }
        fn give_back(&self) {
            self.log.borrow_mut().push((self.id, self.reply.borrow().clone()));
        }
    }

    /// The deferred-element path, as drain and the signal sink take it: a
    /// fenced request stays behind while later unfenced ones are answered,
    /// and is answered (with its own fence id) once the renderer, from
    /// another thread, signals it; a reset answers the rest with errors.
    #[test]
    fn fenced_elements_wait_for_their_signal() {
        use crate::gpu::backend::fake::{FakeBackend, fire};
        use crate::gpu::protocol::{Command, FLAG_FENCE, Request};
        use crate::gpu::server::{GuestMemory, Mapping};
        use std::sync::mpsc;

        struct NoRam;
        impl GuestMemory for NoRam {
            fn map(&self, _: u64, _: usize) -> Option<Box<dyn Mapping>> {
                None
            }
        }
        let (backend, shared) = FakeBackend::new();
        let frames: &'static crate::gpu::frames::Frames = Box::leak(Box::new(crate::gpu::frames::Frames::new()));
        let mut server = Server::new(vec![(64, 64)], vec![], 1 << 20, frames).with_backend(Box::new(backend));
        // The sink posts to "the device queue": here a channel this thread reads.
        let (tx, queue) = mpsc::channel();
        server.set_signal_sink(Box::new(move |s| {
            let _ = tx.send(s);
        }));
        let log = std::rc::Rc::new(RefCell::new(Vec::new()));
        let element = |id, hdr: CtrlHdr, cmd| Fake { id, request: Request { hdr, cmd }.serialize(), room: 4096, log: log.clone(), reply: RefCell::new(vec![]) };
        let fenced = |fence_id| CtrlHdr { flags: FLAG_FENCE, fence_id, ctx_id: 1, ..Default::default() };
        let mut pending = Pending::default();
        let big = 1u64 << 33;
        let batch = [
            element(1, CtrlHdr { ctx_id: 1, ..Default::default() }, Command::CtxCreate { context_init: 0, debug_name: vec![] }),
            element(2, fenced(big), Command::Submit3d { data: vec![] }),
            element(3, CtrlHdr::default(), Command::GetDisplayInfo),
            element(4, fenced(big + 1), Command::Submit3d { data: vec![] }),
        ];
        for e in batch {
            assert!(serve(&mut server, &NoRam, &mut pending, e).is_none());
        }
        let answered = |log: &RefCell<Vec<(u32, Vec<u8>)>>| log.borrow().iter().map(|(id, _)| *id).collect::<Vec<_>>();
        assert_eq!(answered(&log), vec![1, 3], "unfenced ones overtake");
        assert_eq!(pending.len(), 2);

        // The renderer signals the first fence from its own thread.
        let renderer = shared.clone();
        std::thread::spawn(move || fire(&renderer, vec![Signal::Global(1)])).join().unwrap();
        release(&mut pending, &queue.recv().unwrap());
        assert_eq!(answered(&log), vec![1, 3, 2]);
        let (hdr, resp) = Response::parse(&log.borrow()[2].1).unwrap();
        assert_eq!((hdr.fence_id, hdr.flags, resp), (big, FLAG_FENCE, Response::OkNoData));

        // A reset answers what's left with an error, its fence kept.
        assert!(serve(&mut server, &NoRam, &mut pending, element(5, fenced(big + 2), Command::GetDisplayInfo)).is_none());
        fail_all(&mut pending);
        assert!(pending.is_empty());
        assert_eq!(answered(&log), vec![1, 3, 2, 4, 5]);
        let (hdr, resp) = Response::parse(&log.borrow()[3].1).unwrap();
        assert_eq!((hdr.fence_id, resp), (big + 1, Response::Err(cmd::RESP_ERR_UNSPEC)));
        // A late signal for something already answered changes nothing.
        release(&mut pending, &[Signal::Global(3)]);
        assert_eq!(log.borrow().len(), 5);

        // Too large to read, or no room for the reply: still answered.
        serve(&mut server, &NoRam, &mut pending, Fake { id: 6, request: vec![0; MAX_REQUEST + 1], room: 24, log: log.clone(), reply: RefCell::new(vec![]) });
        assert_eq!(Response::parse(&log.borrow()[5].1).unwrap().1, Response::Err(cmd::RESP_ERR_UNSPEC));
        let mut cursor = element(7, CtrlHdr::default(), Command::GetDisplayInfo);
        cursor.room = 0;
        serve(&mut server, &NoRam, &mut pending, cursor);
        assert_eq!(log.borrow()[6], (7, vec![]));
    }

    /// A context destroyed with requests still fenced on its rings: the
    /// renderer drops those fences, so they're answered then.
    #[test]
    fn destroying_a_context_answers_its_ring_fences() {
        use crate::gpu::backend::fake::FakeBackend;
        use crate::gpu::protocol::{Command, F_CONTEXT_INIT, FLAG_FENCE, FLAG_INFO_RING_IDX, Request};
        use crate::gpu::server::{GuestMemory, Mapping};

        struct NoRam;
        impl GuestMemory for NoRam {
            fn map(&self, _: u64, _: usize) -> Option<Box<dyn Mapping>> {
                None
            }
        }
        let frames: &'static crate::gpu::frames::Frames = Box::leak(Box::new(crate::gpu::frames::Frames::new()));
        let mut server = Server::new(vec![(64, 64)], vec![], 1 << 20, frames).with_backend(Box::new(FakeBackend::new().0));
        server.set_negotiated(F_CONTEXT_INIT);
        let log = std::rc::Rc::new(RefCell::new(Vec::new()));
        let element = |id, hdr: CtrlHdr, cmd| Fake { id, request: Request { hdr, cmd }.serialize(), room: 4096, log: log.clone(), reply: RefCell::new(vec![]) };
        let on = |ctx_id| CtrlHdr { ctx_id, ..Default::default() };
        let ring = |ctx_id, fence_id| CtrlHdr { flags: FLAG_FENCE | FLAG_INFO_RING_IDX, fence_id, ctx_id, ring_idx: 1, ..Default::default() };
        let mut pending = Pending::default();
        for e in [
            element(1, on(1), Command::CtxCreate { context_init: 0, debug_name: vec![] }),
            element(2, on(2), Command::CtxCreate { context_init: 0, debug_name: vec![] }),
            element(3, ring(1, 7), Command::Submit3d { data: vec![] }),
            element(4, ring(2, 8), Command::Submit3d { data: vec![] }),
            element(5, on(1), Command::CtxDestroy),
        ] {
            assert!(serve(&mut server, &NoRam, &mut pending, e).is_none());
        }
        let answered: Vec<u32> = log.borrow().iter().map(|(id, _)| *id).collect();
        assert_eq!(answered, vec![1, 2, 3, 5], "ctx 2's fence still waits");
        assert_eq!(Response::parse(&log.borrow()[2].1).unwrap().0.fence_id, 7);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn offered_features() {
        use crate::gpu::backend::fake::FakeBackend;
        let frames: &'static crate::gpu::frames::Frames = Box::leak(Box::new(crate::gpu::frames::Frames::new()));
        let plain = || Server::new(vec![(64, 64)], vec![], 1 << 20, frames);
        let base = protocol::F_EDID | protocol::F_RESOURCE_UUID;
        assert_eq!(features(&plain()), base);
        assert_eq!(features(&plain().with_blobs(1 << 28)), base, "no blobs without a renderer");
        let with = || plain().with_backend(Box::new(FakeBackend::new().0));
        assert_eq!(features(&with()), base | protocol::F_VIRGL);
        assert_eq!(features(&with().with_blobs(1 << 28)), base | protocol::F_VIRGL | protocol::F_CONTEXT_INIT | protocol::F_RESOURCE_BLOB);
        let (mut none, _) = FakeBackend::new();
        none.capsets.clear();
        assert_eq!(features(&plain().with_backend(Box::new(none))), base, "a renderer without capsets is no 3D");
    }
}
