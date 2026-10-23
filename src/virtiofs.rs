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
//! (seconds, default 1), BERTH_FS_WRITEBACK=1, BERTH_FS_POLL_US (default
//! 100), BERTH_FS_QUEUES (request queues, default 1), BERTH_FS_THREADS=1
//! (serve concurrent requests on worker threads), BERTH_FS_STATS=1.
//!
//! Where the time goes: the framework takes ~70 µs to deliver each queue
//! notification to us and about as long to get a reply back to the guest,
//! while serving a request takes ~15 µs (mostly APFS). Polling the queues
//! for 100 µs after they run dry removes the first half for any busy guest,
//! which is what makes this faster than Apple's share. The second half is
//! inside the framework; nothing in the API controls it.
//!
//! Threads don't help: APFS serializes metadata changes (serving goes from
//! 15 to 19 µs per request under concurrency) and the hand-off costs more
//! than it overlaps, so 4 parallel extractions get ~10% slower. Same for
//! extra request queues.
//!
//! Results (M2 Max, 20k x 1 KB files as guest root, 10 s caching), seconds,
//! Apple's share vs this one: extract 7.8 / 7.3, cold read 4.1 / 2.3, delete
//! 2.6 / 2.7; 4 in parallel: extract 19.1 / 12.8, read 8.6 / 3.7, delete
//! 6.6 / 5.6. 2 GiB file: write 1.6 / 2.1 GB/s, read 2.9 / 3.5 GB/s.
//! OrbStack's bind mounts extract the same files in ~2.3 s.

#![allow(non_snake_case)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dispatch2::{DispatchQoS, DispatchQueue, DispatchRetained, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, extern_class, extern_methods, extern_protocol, msg_send};
use objc2_foundation::{NSArray, NSData, NSError};

use crate::fuse::{CachePolicy, Server};

/// virtio-fs device ID; config space is `tag[36]` + `num_request_queues`.
const VIRTIO_ID_FS: u16 = 26;
const TAG: &str = "berthfs";
/// Queue 0 is the high-priority queue (FORGET etc.), then the request
/// queues (Linux 6.11+ spreads CPUs over several, but more didn't help).
const DEFAULT_REQUEST_QUEUES: u16 = 1;
/// Microseconds to keep polling idle queues before waiting for a notification.
const DEFAULT_POLL_US: u64 = 100;

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
        #[unsafe(method(queueIndex))]
        #[unsafe(method_family = none)]
        pub unsafe fn queueIndex(&self) -> u16;
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
    server: Arc<Server>,
    /// The serial queue the framework runs the device on: every queue and
    /// element call happens here.
    device_queue: DispatchRetained<DispatchQueue>,
    /// Where requests are served when several are in flight (None: always
    /// on the device queue).
    workers: Option<DispatchRetained<DispatchQueue>>,
    done: Arc<Completions>,
    /// Every virtqueue seen so far, by index, so one poll covers them all.
    /// Device queue only.
    queues: RefCell<Vec<Option<Retained<VZVirtioQueue>>>>,
}

/// A served request: the job, its reply, and the time spent serving it.
type Finished = (Job, Option<Vec<u8>>, Duration);

/// Replies from workers, waiting to go back to the guest on the device queue.
#[derive(Default)]
struct Completions {
    list: Mutex<Vec<Finished>>,
    in_flight: AtomicUsize,
    /// The device queue is in its polling loop and will pick replies up;
    /// otherwise a worker schedules a flush.
    polling: AtomicBool,
}

impl Completions {
    /// Return finished requests to the guest. Device queue only.
    fn flush(&self) -> bool {
        let done = std::mem::take(&mut *self.list.lock().unwrap());
        let any = !done.is_empty();
        for (job, reply, served) in done {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            job.complete(reply, served);
        }
        any
    }
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
            self.run(queue);
        }
    }
);

/// A request taken off a virtqueue, on its way to a worker and back.
struct Job {
    element: Retained<VZVirtioQueueElement>,
    started: Instant,
}
// SAFETY: the element is only touched on the device queue; workers just
// carry it to the reply.
unsafe impl Send for Job {}

impl Job {
    /// Write the reply and hand the element back to the guest. Device queue only.
    fn complete(self, reply: Option<Vec<u8>>, served: Duration) {
        if let Some(mut reply) = reply {
            let room = unsafe { self.element.writeBuffersByteCount() };
            if reply.len() <= room {
                let _ = unsafe { self.element.writeBuffer_exactLength_error(reply.as_mut_ptr().cast(), reply.len()) };
            }
        }
        unsafe { self.element.returnToQueue() };
        stats::record(served, self.started.elapsed());
    }
}

impl FsDevice {
    fn new(server: Server, device_queue: DispatchRetained<DispatchQueue>, threaded: bool) -> Retained<Self> {
        let workers = threaded.then(|| DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::UserInitiated)));
        let this = Self::alloc().set_ivars(Ivars {
            server: Arc::new(server),
            device_queue,
            workers,
            done: Arc::default(),
            queues: RefCell::new(Vec::new()),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// The device's event loop, entered on a queue notification. Each guest
    /// request otherwise waits ~70 µs for the framework's notification, so
    /// after the queues run dry this keeps polling them (and collecting
    /// worker replies) for a short window: a busy guest's next request is
    /// usually there within microseconds.
    ///
    /// A request is served right here when it's the only one in flight
    /// (handing it to a thread would only add latency); when the guest has
    /// several outstanding they go to worker threads so slow file
    /// operations overlap.
    fn run(&self, notified: &VZVirtioQueue) {
        let ivars = self.ivars();
        let index = unsafe { notified.queueIndex() } as usize;
        stats::notified(index as u16);
        {
            let mut queues = ivars.queues.borrow_mut();
            if queues.len() <= index {
                queues.resize(index + 1, None);
            }
            queues[index] = Some(objc2::Message::retain(notified));
        }
        let queues: Vec<_> = ivars.queues.borrow().iter().flatten().cloned().collect();
        let done = &ivars.done;
        let poll = poll_window();
        done.polling.store(true, Ordering::SeqCst);
        let mut idle_since = Instant::now();
        loop {
            let mut busy = done.flush();
            for queue in &queues {
                busy |= self.drain(queue);
            }
            if busy {
                idle_since = Instant::now();
            } else if idle_since.elapsed() >= poll {
                break;
            }
            std::hint::spin_loop();
        }
        // Workers finishing from here on schedule their own flush; anything
        // that landed before they could see this is picked up now.
        done.polling.store(false, Ordering::SeqCst);
        done.flush();
        if stats::enabled() && stats::due() {
            let ops = ivars.server.opcode_counts();
            eprintln!("virtiofs: by opcode {:?}", ops.iter().take(12).map(|(op, n, us)| format!("{op}:{n}@{us:.1}µs")).collect::<Vec<_>>());
        }
    }

    /// Take every pending request off `queue`. True if there were any.
    fn drain(&self, queue: &VZVirtioQueue) -> bool {
        let ivars = self.ivars();
        let hiprio = unsafe { queue.queueIndex() } == 0;
        let mut next = unsafe { queue.nextElement() };
        let any = next.is_some();
        while let Some(element) = next.take() {
            let started = Instant::now();
            let len = unsafe { element.readBuffersByteCount() };
            let mut request = vec![0u8; len];
            let read = unsafe { element.readBytesIntoBuffer_exactLength_error(request.as_mut_ptr().cast(), len) };
            let job = Job { element, started };
            if read.is_err() {
                job.complete(None, Duration::ZERO);
                next = unsafe { queue.nextElement() };
                continue;
            }
            next = unsafe { queue.nextElement() };
            let concurrent = next.is_some() || ivars.done.in_flight.load(Ordering::SeqCst) > 0;
            match &ivars.workers {
                Some(workers) if concurrent && !hiprio => {
                    ivars.done.in_flight.fetch_add(1, Ordering::SeqCst);
                    let (server, done, device_queue) = (ivars.server.clone(), ivars.done.clone(), ivars.device_queue.clone());
                    workers.exec_async(move || {
                        let t = Instant::now();
                        let reply = server.handle(&request);
                        done.list.lock().unwrap().push((job, reply, t.elapsed()));
                        if !done.polling.load(Ordering::SeqCst) {
                            device_queue.exec_async(move || {
                                done.flush();
                            });
                        }
                    });
                }
                _ => {
                    let t = Instant::now();
                    let reply = ivars.server.handle(&request);
                    job.complete(reply, t.elapsed());
                }
            }
        }
        any
    }
}

/// How long to keep polling once the queues are empty (BERTH_FS_POLL_US).
fn poll_window() -> Duration {
    static US: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_micros(*US.get_or_init(|| {
        std::env::var("BERTH_FS_POLL_US").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_POLL_US)
    }))
}

/// Where time goes per request (BERTH_FS_STATS=1 logs every 20,000).
mod stats {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::time::Duration;

    #[derive(Default)]
    struct Stats {
        n: u64,
        serve: Duration,
        total: Duration,
        logged: u64,
        /// Notifications per virtqueue: shows whether the guest spreads
        /// requests over the request queues.
        queues: [u64; 17],
    }

    static STATS: Mutex<Stats> =
        Mutex::new(Stats { n: 0, serve: Duration::ZERO, total: Duration::ZERO, logged: 0, queues: [0; 17] });

    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("BERTH_FS_STATS").is_some())
    }

    pub fn notified(queue: u16) {
        if enabled() {
            let mut s = STATS.lock().unwrap();
            let i = (queue as usize).min(16);
            s.queues[i] += 1;
        }
    }

    /// `serve`: the server's own time; `total`: off the queue to returned.
    pub fn record(serve: Duration, total: Duration) {
        if !enabled() {
            return;
        }
        let mut s = STATS.lock().unwrap();
        s.n += 1;
        s.serve += serve;
        s.total += total;
    }

    /// True once per 20,000 requests, logging the averages.
    pub fn due() -> bool {
        let mut s = STATS.lock().unwrap();
        if s.n < s.logged + 20_000 {
            return false;
        }
        let us = |d: Duration| d.as_secs_f64() * 1e6 / s.n as f64;
        eprintln!(
            "virtiofs: {} requests, avg µs: serve {:.1}, queue-to-return {:.1}; notifications per queue {:?}",
            s.n,
            us(s.serve),
            us(s.total),
            &s.queues[..s.queues.iter().rposition(|&n| n > 0).map_or(0, |i| i + 1)]
        );
        s.logged = s.n;
        true
    }
}

/// Keeps the delegate alive for the VM's lifetime (the framework holds it
/// weakly).
struct Keep(#[allow(dead_code)] Retained<FsDevice>);
unsafe impl Send for Keep {}
static KEEP: Mutex<Vec<Keep>> = Mutex::new(Vec::new());

/// The device configuration, or None when the API isn't available (macOS < 27)
/// or the experiment is off.
pub fn device(root: &std::path::Path) -> Option<Retained<NSObject>> {
    if std::env::var_os("BERTH_EXPERIMENTAL_FS").is_none() || AnyClass::get(c"VZCustomVirtioDeviceConfiguration").is_none() {
        return None;
    }
    let num = |var: &str, default: f64| std::env::var(var).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(default);
    let policy = CachePolicy {
        entry: Duration::from_secs_f64(num("BERTH_FS_ENTRY_TIMEOUT", 1.0)),
        attr: Duration::from_secs_f64(num("BERTH_FS_ATTR_TIMEOUT", 1.0)),
        writeback: std::env::var_os("BERTH_FS_WRITEBACK").is_some(),
    };
    let request_queues = (num("BERTH_FS_QUEUES", DEFAULT_REQUEST_QUEUES as f64) as u16).clamp(1, 16);
    let threaded = num("BERTH_FS_THREADS", 0.0) != 0.0;
    eprintln!(
        "virtiofs: experimental device '{TAG}' for {} ({policy:?}, {request_queues} request queues, {})",
        root.display(),
        if threaded { "threaded" } else { "single-threaded" }
    );

    // Every guest request waits on this queue waking up: run it at the
    // highest priority, or each round trip pays a slow thread wake-up.
    let queue = DispatchQueue::new_with_target(
        "dev.berth.virtiofs",
        None,
        Some(&DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::UserInteractive))),
    );
    let device = FsDevice::new(Server::new(root.to_path_buf(), policy), queue.clone(), threaded);
    unsafe {
        let mut config_space = vec![0u8; 40];
        config_space[..TAG.len()].copy_from_slice(TAG.as_bytes());
        config_space[36..40].copy_from_slice(&(request_queues as u32).to_le_bytes()); // num_request_queues
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
        config.setVirtioQueueCount(1 + request_queues);
        config.setDeviceSpecificConfiguration(Some(&specific));
        config.setProvider(Some(&provider));
        KEEP.lock().unwrap().push(Keep(device));
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
