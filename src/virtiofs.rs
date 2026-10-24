//! berth's own virtio-fs device on macOS 27's custom Virtio API
//! (VZCustomVirtioDeviceConfiguration), serving FUSE from fuse.rs: how the
//! VM sees your home folder on macOS 27 (tag `home`, in place of Apple's
//! built-in virtiofs; see `home_share`). Apple's share is still used on
//! older macOS or with `"file_sharing": "apple"` in ~/.berth/config.json.
//!
//! The classes are new in macOS 27 and absent from objc2-virtualization, so
//! they're declared here and looked up at runtime.
//!
//! The device is presented as a plain virtio-fs device (ID 26, served by the
//! guest's built-in driver) until the agent has built berth's polling driver
//! for the kernel that's booting (agent/src/kmod.rs); then under berth's
//! own ID, which only that driver binds. A `kmod-trying` marker makes a
//! failed attempt fall back on the next start.
//!
//! Tunables: BERTH_FILE_SHARING=apple|berth, BERTH_FS_ENTRY_TIMEOUT /
//! _ATTR_TIMEOUT / _NEGATIVE_TIMEOUT (seconds, default 1),
//! BERTH_FS_WRITEBACK=1, BERTH_FS_POLL_US (default 100), BERTH_FS_QUEUES
//! (request queues, default 1), BERTH_FS_THREADS=0|1, BERTH_FS_STATS=1,
//! BERTH_FS_TRACE=1 (log every request), BERTH_FS_POLL_DRIVER=0|1 (force the
//! device ID).
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
//!
//! Requests per file are set by the guest kernel and the tool: tar costs 5
//! (LOOKUP, CREATE, WRITE, SETATTR, GETATTR of the directory), Composer ~12
//! (it chmods each file 2-3 times). What's left to cut is our time per
//! request (SETATTR goes through the open file and skips no-op chmods:
//! 21 -> 7 µs) and the framework's per-request latency, which only a guest
//! driver that polls for replies could reduce.

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
/// The same device under its own ID, bound only by berth's polling guest
/// driver (guest/kmod/virtiofs_poll) instead of the built-in one.
const BERTH_FS_DEVICE_ID: u16 = 62;
const TAG: &str = "home";
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
        #[unsafe(method(setSupportsSaveRestore:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setSupportsSaveRestore(&self, supports: bool);
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

        #[optional]
        #[unsafe(method(customVirtioDeviceSaveStateForRestore:))]
        #[unsafe(method_family = none)]
        unsafe fn customVirtioDeviceSaveStateForRestore(&self, device: &VZCustomVirtioDevice) -> Option<Retained<NSData>>;

        #[optional]
        #[unsafe(method(customVirtioDeviceShouldRestore:saveState:))]
        #[unsafe(method_family = none)]
        unsafe fn customVirtioDeviceShouldRestore_saveState(&self, device: &VZCustomVirtioDevice, save_state: &NSData) -> bool;
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

        /// For a VM snapshot (snapshots.rs): what the guest's kernel refers
        /// to. The VM is paused; let requests still being served finish so
        /// none is lost in the saved queues.
        #[unsafe(method_id(customVirtioDeviceSaveStateForRestore:))]
        fn save_state(&self, _device: &VZCustomVirtioDevice) -> Option<Retained<NSData>> {
            let done = &self.ivars().done;
            let deadline = Instant::now() + Duration::from_secs(5);
            while done.in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
                if !done.flush() {
                    std::thread::sleep(Duration::from_micros(100));
                }
            }
            done.flush();
            Some(NSData::with_bytes(&self.ivars().server.save_state()))
        }

        #[unsafe(method(customVirtioDeviceShouldRestore:saveState:))]
        fn restore_state(&self, _device: &VZCustomVirtioDevice, save_state: &NSData) -> bool {
            let ok = self.ivars().server.restore_state(&save_state.to_vec());
            eprintln!("virtiofs: {} the share's state from the snapshot", if ok { "restored" } else { "couldn't restore" });
            ok
        }
    }
);

const LINUX_EIO: i32 = 5;
const LINUX_ERANGE: i32 = 34;

/// A FUSE reply carrying only an error (fuse_out_header).
fn error_header(unique: u64, errno: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&(-errno).to_le_bytes());
    out.extend_from_slice(&unique.to_le_bytes());
    out
}

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
            if reply.len() > room {
                // The guest reads whatever header is in its buffer: say why
                // instead of leaving it to look like success.
                let unique = reply.get(8..16).map_or(0, |u| u64::from_le_bytes(u.try_into().unwrap()));
                reply = error_header(unique, LINUX_ERANGE);
            }
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
        if let Some(idle) = stats::enabled().then(stats::due).flatten() {
            let ops = ivars.server.opcode_counts();
            eprintln!("virtiofs: by opcode{} {:?}", if idle { " (idle)" } else { "" }, ops.iter().map(|(op, n, us)| format!("{op}:{n}@{us:.1}µs")).collect::<Vec<_>>());
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
                job.complete(Some(error_header(0, LINUX_EIO)), Duration::ZERO);
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

/// Where time goes per request (BERTH_FS_STATS=1 logs every 20,000, and
/// on going idle a second after the last log, so a workload gets a snapshot).
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
        logged_at: Option<std::time::Instant>,
        /// Notifications per virtqueue: shows whether the guest spreads
        /// requests over the request queues.
        queues: [u64; 17],
    }

    static STATS: Mutex<Stats> =
        Mutex::new(Stats { n: 0, serve: Duration::ZERO, total: Duration::ZERO, logged: 0, logged_at: None, queues: [0; 17] });

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

    /// Called when the device goes idle: logs the averages every 20,000
    /// requests (Some(false)), or when there are new ones after a second's
    /// quiet (Some(true): a snapshot between workloads).
    pub fn due() -> Option<bool> {
        let mut s = STATS.lock().unwrap();
        let quiet = s.logged_at.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) && s.n > s.logged;
        if s.n < s.logged + 20_000 && !quiet {
            return None;
        }
        s.logged_at = Some(std::time::Instant::now());
        let us = |d: Duration| d.as_secs_f64() * 1e6 / s.n as f64;
        eprintln!(
            "virtiofs: {} requests, avg µs: serve {:.1}, queue-to-return {:.1}; notifications per queue {:?}",
            s.n,
            us(s.serve),
            us(s.total),
            &s.queues[..s.queues.iter().rposition(|&n| n > 0).map_or(0, |i| i + 1)]
        );
        s.logged = s.n;
        Some(quiet)
    }
}

/// Keeps the delegate alive for the VM's lifetime (the framework holds it
/// weakly).
struct Keep(#[allow(dead_code)] Retained<FsDevice>);
unsafe impl Send for Keep {}
static KEEP: Mutex<Vec<Keep>> = Mutex::new(Vec::new());

/// The device configuration, or None when the API isn't available (macOS < 27)
/// or the experiment is off.
/// How the VM gets the home folder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HomeShare {
    /// Apple's built-in virtiofs.
    Apple,
    /// berth's device; `poll_driver` when presented for the polling driver.
    Berth { poll_driver: bool },
}

impl HomeShare {
    /// For snapshots and suspend: a saved state resumes only with the same device.
    pub fn as_str(self) -> &'static str {
        match self {
            HomeShare::Apple => "apple",
            HomeShare::Berth { poll_driver: true } => "berth-poll",
            HomeShare::Berth { poll_driver: false } => "berth",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "apple" => Some(HomeShare::Apple),
            "berth-poll" => Some(HomeShare::Berth { poll_driver: true }),
            "berth" => Some(HomeShare::Berth { poll_driver: false }),
            _ => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            HomeShare::Apple => "Apple's virtiofs",
            HomeShare::Berth { poll_driver: true } => "berth (polling driver)",
            HomeShare::Berth { poll_driver: false } => "berth (built-in driver)",
        }
    }
}

/// What this start uses, for `berth status`.
pub static CURRENT: std::sync::OnceLock<HomeShare> = std::sync::OnceLock::new();
/// Set before the VM is configured to resume a saved state: use exactly the
/// share it was saved with.
pub static FORCED: std::sync::OnceLock<HomeShare> = std::sync::OnceLock::new();

/// Decide how to share home for this start (and leave the marker that makes
/// a failed polling-driver attempt fall back next time).
pub fn home_share(paths: &crate::paths::Paths) -> HomeShare {
    if let Some(&forced) = FORCED.get() {
        let _ = CURRENT.set(forced);
        return forced;
    }
    #[derive(serde::Deserialize, Default)]
    struct Config {
        file_sharing: Option<String>,
    }
    let setting = std::env::var("BERTH_FILE_SHARING").ok().or_else(|| {
        std::fs::read(paths.config())
            .ok()
            .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
            .and_then(|c| c.file_sharing)
    });
    let share = if setting.as_deref() == Some("apple")
        || AnyClass::get(c"VZCustomVirtioDeviceConfiguration").is_none()
        || crate::vm::safe_boot(paths)
    {
        HomeShare::Apple
    } else {
        HomeShare::Berth { poll_driver: poll_driver_ready(paths) }
    };
    let _ = CURRENT.set(share);
    share
}

/// The guest has the polling driver for the kernel about to boot, and the
/// last attempt with it came up.
fn poll_driver_ready(paths: &crate::paths::Paths) -> bool {
    let guest = paths.guest();
    let (ready, release, trying) = (guest.join("kmod-ready"), guest.join("boot/release"), guest.join("boot/kmod-trying"));
    if let Ok(forced) = std::env::var("BERTH_FS_POLL_DRIVER") {
        return forced != "0";
    }
    if trying.exists() {
        eprintln!("virtiofs: the polling driver didn't come up last time; using the built-in one");
        let _ = std::fs::remove_file(&trying);
        let _ = std::fs::remove_file(&ready);
        return false;
    }
    let read = |p: &std::path::Path| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    let ok = matches!((read(&ready), read(&release)), (Some(a), Some(b)) if !a.is_empty() && a == b);
    if ok {
        let _ = std::fs::write(&trying, "");
    }
    ok
}

fn device(root: &std::path::Path, poll_driver: bool) -> Retained<NSObject> {
    let num = |var: &str, default: f64| std::env::var(var).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(default);
    let policy = CachePolicy {
        entry: Duration::from_secs_f64(num("BERTH_FS_ENTRY_TIMEOUT", 1.0)),
        attr: Duration::from_secs_f64(num("BERTH_FS_ATTR_TIMEOUT", 1.0)),
        negative: Duration::from_secs_f64(num("BERTH_FS_NEGATIVE_TIMEOUT", 1.0)),
        writeback: std::env::var_os("BERTH_FS_WRITEBACK").is_some(),
    };
    let request_queues = (num("BERTH_FS_QUEUES", DEFAULT_REQUEST_QUEUES as f64) as u16).clamp(1, 16);
    // With the polling guest driver the transport is cheap enough that our
    // serving thread becomes the limit for parallel work.
    let threaded = num("BERTH_FS_THREADS", if poll_driver { 1.0 } else { 0.0 }) != 0.0;
    eprintln!(
        "virtiofs: sharing {} as '{TAG}' ({}; {policy:?}, {request_queues} request queues, {})",
        root.display(),
        if poll_driver { "polling driver" } else { "built-in driver" },
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
        config.setDeviceID(if poll_driver { BERTH_FS_DEVICE_ID } else { VIRTIO_ID_FS });
        config.setPCIClassID(0x01); // mass storage
        config.setPCISubclassID(0x80);
        config.setVirtioQueueCount(1 + request_queues);
        config.setDeviceSpecificConfiguration(Some(&specific));
        config.setProvider(Some(&provider));
        config.setSupportsSaveRestore(true);
        KEEP.lock().unwrap().push(Keep(device));
        Retained::into_super(config)
    }
}

/// Add berth's file sharing device for `root` to a VM configuration.
pub fn attach(config: &objc2_virtualization::VZVirtualMachineConfiguration, root: &std::path::Path, poll_driver: bool) {
    {
        let devices = NSArray::from_retained_slice(&[device(root, poll_driver)]);
        unsafe {
            let _: () = msg_send![config, setCustomVirtioDevices: &*devices];
        }
    }
}
