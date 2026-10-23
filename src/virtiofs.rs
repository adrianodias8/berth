//! Experimental: berth's own virtio-fs device on macOS 27's custom Virtio API
//! (VZCustomVirtioDeviceConfiguration), serving FUSE from fuse.rs. Apple's
//! built-in virtiofs doesn't let us choose caching; this does.
//!
//! The classes are new in macOS 27 and absent from objc2-virtualization, so
//! they're declared here and looked up at runtime: on older systems the
//! device is simply not added.
//!
//! Enabled with BERTH_EXPERIMENTAL_FS=1. The share appears in the guest under
//! the tag `berthfs` (mount -t virtiofs berthfs /mnt/berthfs), next to the
//! regular `home` share. Tunables: BERTH_FS_ENTRY_TIMEOUT / _ATTR_TIMEOUT
//! (seconds, default 1), BERTH_FS_WRITEBACK=1, BERTH_FS_STATS=1.
//!
//! Spike results (M2 Max, 20k x 1 KB files, inside the guest), vs Apple's:
//! - one process: read 1.8-2.5x faster (FLUSH/OPEN eliminated, READDIRPLUS,
//!   longer caching); extract and delete ~1.4x slower. Each request costs
//!   ~20 µs more than Apple's in-process server: the guest runs in Apple's
//!   VM process and every queue notification crosses into ours.
//! - 4 processes: extract 1.3x faster (requests batch per notification),
//!   delete 1.16x slower. Our serving thread is ~60% busy there, so
//!   multithreading would add at most ~1.6x.
//! Not a consistent win yet; Apple's share stays the default.

#![allow(non_snake_case)]

use std::sync::Mutex;

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, extern_class, extern_methods, extern_protocol, msg_send};
use objc2_foundation::{NSArray, NSData, NSError};

use crate::fuse::{CachePolicy, Server};

/// virtio-fs device ID; config space is `tag[36]` + `num_request_queues`.
const VIRTIO_ID_FS: u16 = 26;
const TAG: &str = "berthfs";
/// Queue 0 is the high-priority queue (FORGET etc.), queue 1 the requests.
const QUEUES: u16 = 2;

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZVirtioDeviceSpecificConfiguration;
);
impl VZVirtioDeviceSpecificConfiguration {
    extern_methods!(
        #[unsafe(method(initWithConfigurationData:))]
        #[unsafe(method_family = init)]
        pub unsafe fn initWithConfigurationData(this: objc2::rc::Allocated<Self>, data: &NSData) -> Retained<Self>;
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZCustomVirtioDeviceProvider;
);

extern_class!(
    #[unsafe(super(VZCustomVirtioDeviceProvider))]
    pub struct VZCustomVirtioDeviceDelegateProvider;
);
impl VZCustomVirtioDeviceDelegateProvider {
    extern_methods!(
        #[unsafe(method(initWithDeviceQueue:delegate:))]
        #[unsafe(method_family = init)]
        pub unsafe fn initWithDeviceQueue_delegate(
            this: objc2::rc::Allocated<Self>,
            queue: &DispatchQueue,
            delegate: &ProtocolObject<dyn VZCustomVirtioDeviceConfigurationDelegate>,
        ) -> Retained<Self>;
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZCustomVirtioDeviceConfiguration;
);
impl VZCustomVirtioDeviceConfiguration {
    extern_methods!(
        #[unsafe(method(new))]
        #[unsafe(method_family = new)]
        pub unsafe fn new() -> Retained<Self>;
        #[unsafe(method(setDeviceID:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setDeviceID(&self, id: u16);
        #[unsafe(method(setPCIClassID:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setPCIClassID(&self, id: u8);
        #[unsafe(method(setPCISubclassID:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setPCISubclassID(&self, id: u8);
        #[unsafe(method(setVirtioQueueCount:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setVirtioQueueCount(&self, count: u16);
        #[unsafe(method(setDeviceSpecificConfiguration:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setDeviceSpecificConfiguration(&self, config: Option<&VZVirtioDeviceSpecificConfiguration>);
        #[unsafe(method(setProvider:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setProvider(&self, provider: Option<&VZCustomVirtioDeviceProvider>);
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZVirtioQueueElement;
);
impl VZVirtioQueueElement {
    extern_methods!(
        #[unsafe(method(readBuffersByteCount))]
        #[unsafe(method_family = none)]
        pub unsafe fn readBuffersByteCount(&self) -> usize;
        #[unsafe(method(writeBuffersByteCount))]
        #[unsafe(method_family = none)]
        pub unsafe fn writeBuffersByteCount(&self) -> usize;
        #[unsafe(method(readBytesIntoBuffer:exactLength:error:_))]
        #[unsafe(method_family = none)]
        pub unsafe fn readBytesIntoBuffer_exactLength_error(
            &self,
            buffer: *mut std::ffi::c_void,
            length: usize,
        ) -> Result<(), Retained<NSError>>;
        #[unsafe(method(writeBuffer:exactLength:error:_))]
        #[unsafe(method_family = none)]
        pub unsafe fn writeBuffer_exactLength_error(
            &self,
            buffer: *mut std::ffi::c_void,
            length: usize,
        ) -> Result<(), Retained<NSError>>;
        #[unsafe(method(returnToQueue))]
        #[unsafe(method_family = none)]
        pub unsafe fn returnToQueue(&self);
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZVirtioQueue;
);
impl VZVirtioQueue {
    extern_methods!(
        #[unsafe(method(nextElement))]
        #[unsafe(method_family = none)]
        pub unsafe fn nextElement(&self) -> Option<Retained<VZVirtioQueueElement>>;
    );
}

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZCustomVirtioDevice;
);
impl VZCustomVirtioDevice {
    extern_methods!(
        #[unsafe(method(setDelegate:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setDelegate(&self, delegate: Option<&ProtocolObject<dyn VZCustomVirtioDeviceDelegate>>);
    );
}

extern_protocol!(
    /// # Safety
    /// Implementations must match the Objective-C protocol's signatures.
    pub unsafe trait VZCustomVirtioDeviceConfigurationDelegate: NSObjectProtocol {
        #[unsafe(method(customVirtioConfiguration:didCreateDevice:))]
        #[unsafe(method_family = none)]
        unsafe fn customVirtioConfiguration_didCreateDevice(
            &self,
            configuration: &VZCustomVirtioDeviceConfiguration,
            device: &VZCustomVirtioDevice,
        );
    }
);

extern_protocol!(
    /// # Safety
    /// Implementations must match the Objective-C protocol's signatures.
    pub unsafe trait VZCustomVirtioDeviceDelegate: NSObjectProtocol {
        #[unsafe(method(customVirtioDevice:didReceiveNotificationForQueue:))]
        #[unsafe(method_family = none)]
        unsafe fn customVirtioDevice_didReceiveNotificationForQueue(
            &self,
            device: &VZCustomVirtioDevice,
            queue: &VZVirtioQueue,
        );
    }
);

pub struct Ivars {
    server: Mutex<Server>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this type has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "BerthVirtioFsDevice"]
    #[ivars = Ivars]
    pub struct FsDevice;

    unsafe impl NSObjectProtocol for FsDevice {}

    unsafe impl VZCustomVirtioDeviceConfigurationDelegate for FsDevice {
        #[unsafe(method(customVirtioConfiguration:didCreateDevice:))]
        fn did_create(&self, _configuration: &VZCustomVirtioDeviceConfiguration, device: &VZCustomVirtioDevice) {
            unsafe { device.setDelegate(Some(ProtocolObject::from_ref(self))) };
            eprintln!("virtiofs: device created");
        }
    }

    unsafe impl VZCustomVirtioDeviceDelegate for FsDevice {
        #[unsafe(method(customVirtioDevice:didReceiveNotificationForQueue:))]
        fn did_notify(&self, _device: &VZCustomVirtioDevice, queue: &VZVirtioQueue) {
            self.drain(queue);
        }
    }
);

impl FsDevice {
    fn new(server: Server) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { server: Mutex::new(server) });
        unsafe { msg_send![super(this), init] }
    }

    /// Serve every pending request. Runs on the device queue; notifications
    /// stay off until the queue is drained, so loop to empty.
    fn drain(&self, queue: &VZVirtioQueue) {
        use std::time::Instant;
        let mut server = self.ivars().server.lock().unwrap();
        let mut request = Vec::new();
        loop {
            let t0 = Instant::now();
            let Some(element) = (unsafe { queue.nextElement() }) else { break };
            let t1 = Instant::now();
            let len = unsafe { element.readBuffersByteCount() };
            request.resize(len, 0);
            let read = unsafe { element.readBytesIntoBuffer_exactLength_error(request.as_mut_ptr().cast(), len) };
            let t2 = Instant::now();
            let reply = if read.is_ok() { server.handle(&request) } else { None };
            let t3 = Instant::now();
            if let Some(mut reply) = reply {
                let room = unsafe { element.writeBuffersByteCount() };
                if reply.len() <= room {
                    let _ = unsafe { element.writeBuffer_exactLength_error(reply.as_mut_ptr().cast(), reply.len()) };
                }
            }
            unsafe { element.returnToQueue() };
            let t4 = Instant::now();
            let mut stats = STATS.lock().unwrap();
            stats.record(t1 - t0, t2 - t1, t3 - t2, t4 - t3);
            if stats.n > 0 && stats.n.is_multiple_of(20_000) {
                let mut ops: Vec<_> = server.opcodes.iter().map(|(k, v)| (*v, *k)).collect();
                ops.sort_unstable_by(|a, b| b.cmp(a));
                eprintln!("virtiofs: by opcode {:?}", ops.iter().take(12).map(|(n, op)| format!("{op}:{n}")).collect::<Vec<_>>());
            }
        }
    }
}

/// Where time goes per request (BERTH_FS_STATS=1 logs every 20,000).
#[derive(Default)]
struct Stats {
    n: u64,
    next: std::time::Duration,
    read: std::time::Duration,
    serve: std::time::Duration,
    reply: std::time::Duration,
}

impl Stats {
    fn record(&mut self, next: std::time::Duration, read: std::time::Duration, serve: std::time::Duration, reply: std::time::Duration) {
        if std::env::var_os("BERTH_FS_STATS").is_none() {
            return;
        }
        self.n += 1;
        self.next += next;
        self.read += read;
        self.serve += serve;
        self.reply += reply;
        if self.n.is_multiple_of(20_000) {
            let us = |d: std::time::Duration| d.as_secs_f64() * 1e6 / self.n as f64;
            eprintln!(
                "virtiofs: {} requests, avg µs: nextElement {:.1}, read {:.1}, serve {:.1}, write+return {:.1}",
                self.n, us(self.next), us(self.read), us(self.serve), us(self.reply)
            );
        }
    }
}

static STATS: Mutex<Stats> = Mutex::new(Stats {
    n: 0,
    next: std::time::Duration::ZERO,
    read: std::time::Duration::ZERO,
    serve: std::time::Duration::ZERO,
    reply: std::time::Duration::ZERO,
});

/// Keeps the delegate and its queue alive for the VM's lifetime (the
/// framework holds the delegate weakly).
struct Keep(#[allow(dead_code)] Retained<FsDevice>, #[allow(dead_code)] DispatchRetained<DispatchQueue>);
unsafe impl Send for Keep {}
static KEEP: Mutex<Vec<Keep>> = Mutex::new(Vec::new());

/// The device configuration, or None when the API isn't available (macOS < 27)
/// or the experiment is off.
pub fn device(root: &std::path::Path) -> Option<Retained<NSObject>> {
    if std::env::var_os("BERTH_EXPERIMENTAL_FS").is_none() || AnyClass::get(c"VZCustomVirtioDeviceConfiguration").is_none() {
        return None;
    }
    let secs = |var: &str, default: f64| {
        std::env::var(var).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(default)
    };
    let policy = CachePolicy {
        entry: std::time::Duration::from_secs_f64(secs("BERTH_FS_ENTRY_TIMEOUT", 1.0)),
        attr: std::time::Duration::from_secs_f64(secs("BERTH_FS_ATTR_TIMEOUT", 1.0)),
        writeback: std::env::var_os("BERTH_FS_WRITEBACK").is_some(),
    };
    eprintln!("virtiofs: experimental device '{TAG}' for {} ({policy:?})", root.display());

    let device = FsDevice::new(Server::new(root.to_path_buf(), policy));
    let queue = DispatchQueue::new("dev.berth.virtiofs", None);
    unsafe {
        let mut config_space = vec![0u8; 40];
        config_space[..TAG.len()].copy_from_slice(TAG.as_bytes());
        config_space[36..40].copy_from_slice(&1u32.to_le_bytes()); // num_request_queues
        let specific = VZVirtioDeviceSpecificConfiguration::initWithConfigurationData(
            VZVirtioDeviceSpecificConfiguration::alloc(),
            &NSData::with_bytes(&config_space),
        );
        let provider = VZCustomVirtioDeviceDelegateProvider::initWithDeviceQueue_delegate(
            VZCustomVirtioDeviceDelegateProvider::alloc(),
            &queue,
            ProtocolObject::from_ref(&*device),
        );
        let config = VZCustomVirtioDeviceConfiguration::new();
        config.setDeviceID(VIRTIO_ID_FS);
        config.setPCIClassID(0x01); // mass storage
        config.setPCISubclassID(0x80);
        config.setVirtioQueueCount(QUEUES);
        config.setDeviceSpecificConfiguration(Some(&specific));
        config.setProvider(Some(&provider));
        KEEP.lock().unwrap().push(Keep(device, queue));
        Some(Retained::into_super(config))
    }
}

/// Attach the device (if any) to a VM configuration.
pub fn attach(config: &objc2_virtualization::VZVirtualMachineConfiguration, root: &std::path::Path) {
    if let Some(device) = device(root) {
        let devices = NSArray::from_retained_slice(&[device]);
        unsafe {
            let _: () = msg_send![config, setCustomVirtioDevices: &*devices];
        }
    }
}
