//! Virtualization.framework for macOS guests: the configuration
//! (VZMacPlatformConfiguration and friends), restore images and the
//! installer, and `berth mac run`, the process that holds one running VM.
//!
//! That process works as docs/gui-protocol.md ("Transport") says: the VM is
//! created on the main dispatch queue, which NSApplication's run loop drains
//! on the main thread, so a window with a VZVirtualMachineView can be opened
//! at start (`--display`) or later (`{"op":"display"}`). Without a window
//! the app has no Dock icon. `control.sock` and `agent.sock` are served by
//! plain threads that hop onto the main queue for anything touching the VM.
//!
//! Guest provisioning (VZMacGuestProvisioningOptions, macOS 27) isn't in
//! objc2-virtualization 0.3.2, so it's declared here with `extern_class!`
//! and looked up at runtime, as virtiofs.rs does for the custom Virtio API.

#![allow(non_snake_case)]

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, ProtocolObject};
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly, define_class, extern_class, extern_methods, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSScreen, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSError, NSObject, NSObjectProtocol, NSPoint, NSProgress, NSRect, NSSize, NSString, NSURL};
use objc2_virtualization::*;

use super::store::{Hardware, Meta, Vm};

/// Guest vsock port of berth-gui-mac; `agent.sock` connections go there.
pub const GUEST_AGENT_PORT: u32 = 2401;
/// How long a guest gets to shut down after a stop request before it's
/// stopped hard. macOS may first ask the logged-in user to confirm.
pub const STOP_GRACE: Duration = Duration::from_secs(60);
/// The push folder's name in the guest: /Volumes/My Shared Files/berth-push.
pub const PUSH_SHARE: &str = "berth-push";

extern_class!(
    #[unsafe(super(NSObject))]
    pub struct VZGuestProvisioningOptions;
);

extern_class!(
    #[unsafe(super(VZGuestProvisioningOptions, NSObject))]
    pub struct VZMacGuestProvisioningOptions;
);
impl VZMacGuestProvisioningOptions {
    extern_methods!(
        #[unsafe(method(new))]
        #[unsafe(method_family = new)]
        pub unsafe fn new() -> Retained<Self>;
        #[unsafe(method(setFullName:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setFullName(&self, name: &NSString);
        #[unsafe(method(setUsername:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setUsername(&self, name: &NSString);
        #[unsafe(method(setPassword:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setPassword(&self, password: &NSString);
        #[unsafe(method(setLogsInAutomatically:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setLogsInAutomatically(&self, yes: bool);
        #[unsafe(method(setEnablesRemoteLogin:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setEnablesRemoteLogin(&self, yes: bool);
    );
}

/// The account `provision.json` asks for (written by `berth mac create`).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Provision {
    pub full_name: String,
    pub username: String,
    pub password: String,
}

fn file_url(path: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn ns_error(err: &NSError) -> anyhow::Error {
    anyhow!("{}", err.localizedDescription())
}

/// A restore image found on Apple's servers.
pub struct RemoteImage {
    pub url: String,
    pub build: String,
    pub version: String,
    /// Hardware model, minimum CPUs and minimum memory (MiB), when Apple's
    /// catalog says this Mac can run it.
    pub requirements: Option<(Vec<u8>, usize, u64)>,
}

/// The newest macOS this Mac can run in a VM (metadata only; nothing is
/// downloaded).
pub fn latest_restore_image() -> Result<RemoteImage> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |image: *mut VZMacOSRestoreImage, err: *mut NSError| {
        let result = match (unsafe { image.as_ref() }, unsafe { err.as_ref() }) {
            (Some(image), None) => unsafe {
                let v = image.operatingSystemVersion();
                Ok(RemoteImage {
                    url: image.URL().absoluteString().map(|s| s.to_string()).unwrap_or_default(),
                    build: image.buildVersion().to_string(),
                    version: format!("{}.{}.{}", v.majorVersion, v.minorVersion, v.patchVersion),
                    requirements: image.mostFeaturefulSupportedConfiguration().map(|req| {
                        (
                            req.hardwareModel().dataRepresentation().to_vec(),
                            req.minimumSupportedCPUCount(),
                            req.minimumSupportedMemorySize() >> 20,
                        )
                    }),
                })
            },
            (_, Some(err)) => Err(err.localizedDescription().to_string()),
            _ => Err("no restore image".into()),
        };
        let _ = tx.send(result);
    });
    unsafe { VZMacOSRestoreImage::fetchLatestSupportedWithCompletionHandler(&handler) };
    rx.recv_timeout(Duration::from_secs(60))
        .context("Apple's restore image service didn't answer")?
        .map_err(|e| anyhow!("can't find the latest macOS restore image: {e}"))
}

/// What a local IPSW says about the VM it needs.
pub struct LocalImage {
    pub hardware_model: Vec<u8>,
    pub min_cpus: usize,
    pub min_memory_mib: u64,
    pub build: String,
    pub version: String,
}

pub fn load_restore_image(path: &std::path::Path) -> Result<LocalImage> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |image: *mut VZMacOSRestoreImage, err: *mut NSError| {
        let result = match (unsafe { image.as_ref() }, unsafe { err.as_ref() }) {
            (Some(image), None) => unsafe {
                let v = image.operatingSystemVersion();
                let version = format!("{}.{}.{}", v.majorVersion, v.minorVersion, v.patchVersion);
                match image.mostFeaturefulSupportedConfiguration() {
                    None => Err(format!("this Mac can't run macOS {version} ({}) in a VM", image.buildVersion())),
                    Some(req) if !req.hardwareModel().isSupported() => {
                        Err(format!("this Mac doesn't support macOS {version}'s virtual hardware"))
                    }
                    Some(req) => Ok(LocalImage {
                        hardware_model: req.hardwareModel().dataRepresentation().to_vec(),
                        min_cpus: req.minimumSupportedCPUCount(),
                        min_memory_mib: req.minimumSupportedMemorySize() >> 20,
                        build: image.buildVersion().to_string(),
                        version,
                    }),
                }
            },
            (_, Some(err)) => Err(err.localizedDescription().to_string()),
            _ => Err("no restore image".into()),
        };
        let _ = tx.send(result);
    });
    unsafe { VZMacOSRestoreImage::loadFileURL_completionHandler(&file_url(path), &handler) };
    rx.recv_timeout(Duration::from_secs(120))
        .context("loading the restore image timed out")?
        .map_err(|e| anyhow!("{}: {e}", path.display()))
}

/// A fresh identity for a new VM or a clone: its own machine identifier and
/// MAC address, with `hardware_model` (the base's, for a clone).
pub fn new_hardware(hardware_model: &[u8]) -> Hardware {
    let id = unsafe { VZMacMachineIdentifier::new() };
    let mac = unsafe { VZMACAddress::randomLocallyAdministeredAddress() };
    Hardware::new(hardware_model, &unsafe { id.dataRepresentation() }.to_vec(), &unsafe { mac.string() }.to_string())
}

/// The VM's NVRAM, made for its hardware model.
pub fn create_aux(vm: &Vm, hw: &Hardware) -> Result<()> {
    let model = hardware_model(hw)?;
    unsafe {
        VZMacAuxiliaryStorage::initCreatingStorageAtURL_hardwareModel_options_error(
            VZMacAuxiliaryStorage::alloc(),
            &file_url(&vm.aux()),
            &model,
            VZMacAuxiliaryStorageInitializationOptions::AllowOverwrite,
        )
    }
    .map_err(|e| ns_error(&e))
    .context("creating the auxiliary storage")?;
    Ok(())
}

fn hardware_model(hw: &Hardware) -> Result<Retained<VZMacHardwareModel>> {
    let data = NSData::with_bytes(&hw.hardware_model_bytes()?);
    let model = unsafe { VZMacHardwareModel::initWithDataRepresentation(VZMacHardwareModel::alloc(), &data) }
        .context("hw.json: unreadable hardware model")?;
    if !unsafe { model.isSupported() } {
        bail!("this Mac can't run this VM's hardware model (made for a newer macOS?)");
    }
    Ok(model)
}

/// The whole configuration, validated. Any thread; the VM made from it
/// must then stay on one queue.
pub fn build_config(vm: &Vm, meta: &Meta, hw: &Hardware) -> Result<Retained<VZVirtualMachineConfiguration>> {
    let model = hardware_model(hw)?;
    let id_data = NSData::with_bytes(&hw.machine_identifier_bytes()?);
    unsafe {
        let config = VZVirtualMachineConfiguration::new();
        config.setCPUCount(meta.cpus);
        config.setMemorySize(meta.memory_mib << 20);

        let platform = VZMacPlatformConfiguration::new();
        let id = VZMacMachineIdentifier::initWithDataRepresentation(VZMacMachineIdentifier::alloc(), &id_data)
            .context("hw.json: unreadable machine identifier")?;
        platform.setHardwareModel(&model);
        platform.setMachineIdentifier(&id);
        let aux = VZMacAuxiliaryStorage::initWithURL(VZMacAuxiliaryStorage::alloc(), &file_url(&vm.aux()));
        platform.setAuxiliaryStorage(Some(&aux));
        config.setPlatform(&platform);
        config.setBootLoader(Some(&VZMacOSBootLoader::new()));

        // One display, a fixed size: agents' coordinates and screenshots stay
        // the same whatever window (if any) shows it.
        let d = meta.display;
        let display = VZMacGraphicsDisplayConfiguration::initWithWidthInPixels_heightInPixels_pixelsPerInch(
            VZMacGraphicsDisplayConfiguration::alloc(),
            d.width as isize,
            d.height as isize,
            d.ppi as isize,
        );
        let graphics = VZMacGraphicsDeviceConfiguration::new();
        graphics.setDisplays(&NSArray::from_retained_slice(&[display]));
        config.setGraphicsDevices(&NSArray::from_retained_slice(&[Retained::into_super(graphics)]));
        config.setKeyboards(&NSArray::from_retained_slice(&[Retained::into_super(VZMacKeyboardConfiguration::new())]));
        config.setPointingDevices(&NSArray::from_retained_slice(&[Retained::into_super(VZMacTrackpadConfiguration::new())]));

        let disk = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &file_url(&vm.disk()),
            false,
            VZDiskImageCachingMode::Cached,
            VZDiskImageSynchronizationMode::Fsync,
        )
        .map_err(|e| ns_error(&e))
        .context("attaching the disk")?;
        let block = VZVirtioBlockDeviceConfiguration::initWithAttachment(VZVirtioBlockDeviceConfiguration::alloc(), &disk);
        config.setStorageDevices(&NSArray::from_retained_slice(&[Retained::into_super(block)]));

        let mac = VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(&hw.mac_address))
            .context("hw.json: bad mac_address")?;
        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setMACAddress(&mac);
        net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        config.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));

        config.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(VZVirtioSocketDeviceConfiguration::new())]));

        // The push folder. macOS guests mount devices with the automount tag
        // by themselves, at /Volumes/My Shared Files/<name> for each name.
        std::fs::create_dir_all(vm.push())?;
        let push = VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &file_url(&vm.push()), false);
        let dirs = NSDictionary::from_slices(&[&*NSString::from_str(PUSH_SHARE)], &[&*push]);
        let share = VZMultipleDirectoryShare::initWithDirectories(VZMultipleDirectoryShare::alloc(), &dirs);
        let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &VZVirtioFileSystemDeviceConfiguration::macOSGuestAutomountTag(),
        );
        fs.setShare(Some(&share));
        config.setDirectorySharingDevices(&NSArray::from_retained_slice(&[Retained::into_super(fs)]));

        config.validateWithError().map_err(|e| ns_error(&e)).context("invalid macOS VM configuration")?;
        Ok(config)
    }
}

/// Objects made on one queue and only used there.
struct OnQueue<T>(T);
unsafe impl<T> Send for OnQueue<T> {}

/// Install `ipsw` onto the VM's (empty) disk, printing progress. The VM
/// lives on its own serial queue: this process has no run loop.
pub fn install(vm: &Vm, meta: &Meta, hw: &Hardware, ipsw: &std::path::Path) -> Result<()> {
    let config = OnQueue(build_config(vm, meta, hw)?);
    let queue = DispatchQueue::new("dev.berth.mac.install", None);
    let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
    let (progress_tx, progress_rx) = mpsc::channel::<OnQueue<Retained<NSProgress>>>();
    let ipsw = ipsw.to_path_buf();
    // Kept until the process exits: the installer and its VM must outlive
    // the block that starts them.
    static KEEP: Mutex<Vec<OnQueue<Retained<NSObject>>>> = Mutex::new(Vec::new());
    let q = queue.clone();
    queue.exec_async(move || {
        let config = config;
        unsafe {
            let machine = VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &config.0, &q);
            let installer = VZMacOSInstaller::initWithVirtualMachine_restoreImageURL(VZMacOSInstaller::alloc(), &machine, &file_url(&ipsw));
            let _ = progress_tx.send(OnQueue(installer.progress()));
            let handler = RcBlock::new(move |err: *mut NSError| {
                let _ = done_tx.send(err.as_ref().map_or(Ok(()), |e| Err(e.localizedDescription().to_string())));
            });
            installer.installWithCompletionHandler(&handler);
            let mut keep = KEEP.lock().unwrap();
            keep.push(OnQueue(Retained::into_super(machine)));
            keep.push(OnQueue(Retained::into_super(installer)));
        }
    });
    let progress = progress_rx.recv_timeout(Duration::from_secs(60)).context("the installer didn't start")?;
    let mut shown = -1i64;
    let result = loop {
        match done_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // NSProgress is thread-safe.
                let pct = (progress.0.fractionCompleted() * 100.0) as i64;
                if pct != shown {
                    shown = pct;
                    print!("\rInstalling macOS: {pct}%");
                    let _ = std::io::stdout().flush();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err("the installer went away".into()),
        }
    };
    println!();
    result.map_err(|e| anyhow!("installing macOS failed: {e}"))
}

// ---- `berth mac run`: the process that holds one running VM ----

/// The VM, touched only on the main queue (see vm.rs for why not a thread_local).
struct MainVm(Retained<VZVirtualMachine>);
unsafe impl Send for MainVm {}
static VM: Mutex<Option<MainVm>> = Mutex::new(None);
static THIS: OnceLock<Vm> = OnceLock::new();
static DISPLAY: OnceLock<super::store::Display> = OnceLock::new();
static STARTED: AtomicU64 = AtomicU64::new(0);
static STOPPING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The display window, made on first use. Main thread only.
    static WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this type has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "BerthMacVmDelegate"]
    struct VmDelegate;

    unsafe impl NSObjectProtocol for VmDelegate {}

    unsafe impl VZVirtualMachineDelegate for VmDelegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            eprintln!("guest shut down");
            finish(0);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, err: &NSError) {
            eprintln!("VM stopped with an error: {}", err.localizedDescription());
            finish(1);
        }
    }
);

impl VmDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// The VM is gone: take down the files that say it runs, and exit.
fn finish(code: i32) -> ! {
    if let Some(vm) = THIS.get() {
        for path in [vm.control(), vm.agent(), vm.pid()] {
            let _ = std::fs::remove_file(path);
        }
    }
    std::process::exit(code)
}

/// Run `f` on the main queue (the VM's and AppKit's) and wait for it.
fn on_main<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let (tx, rx) = mpsc::channel();
    DispatchQueue::main().exec_async(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(30)).context("the VM's queue didn't answer")
}

/// Run the VM until it stops; never returns once the VM is created.
pub fn run(vm: &Vm, display: bool) -> Result<()> {
    let meta = vm.load_meta()?;
    let hw = vm.load_hw()?;
    if !vm.disk().exists() || !vm.aux().exists() {
        bail!("{} has no disk (was its install interrupted? delete it and create it again)", vm.name);
    }
    let mtm = MainThreadMarker::new().context("berth mac run must be on the main thread")?;
    // Block the stop signals before any thread exists, so they all inherit
    // the mask and only the signal thread sees them.
    let signals = stop_signals();
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) };

    let pid = vm.lock_pid()?;
    std::mem::forget(pid);
    for sock in [vm.control(), vm.agent()] {
        let _ = std::fs::remove_file(sock);
    }
    let _ = THIS.set(vm.clone());
    let _ = DISPLAY.set(meta.display);
    // Before the VM starts: a path too long for a socket (104 bytes) fails here.
    let control = UnixListener::bind(vm.control()).with_context(|| format!("binding {}", vm.control().display()))?;
    let agent = UnixListener::bind(vm.agent()).with_context(|| format!("binding {}", vm.agent().display()))?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let config = build_config(vm, &meta, &hw).inspect_err(|_| {
        for path in [vm.control(), vm.agent(), vm.pid()] {
            let _ = std::fs::remove_file(path);
        }
    })?;
    let machine = unsafe { VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &config) };
    let delegate = VmDelegate::new();
    unsafe { machine.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    // The VM holds its delegate weakly.
    std::mem::forget(delegate);

    let options = unsafe { VZMacOSVirtualMachineStartOptions::new() };
    let provisioning = std::fs::read(vm.provision()).ok().and_then(|b| serde_json::from_slice::<Provision>(&b).ok());
    if let Some(p) = &provisioning {
        provision(&options, p)?;
        eprintln!("first boot: creating user {} (logs in automatically, Remote Login on)", p.username);
    }
    let provision_file = vm.provision();
    let started = RcBlock::new(move |err: *mut NSError| match unsafe { err.as_ref() } {
        Some(err) => {
            eprintln!("VM failed to start: {}", err.localizedDescription());
            finish(1);
        }
        None => {
            STARTED.store(super::store::now(), Ordering::SeqCst);
            // macOS only reads the account on the first boot after install.
            let _ = std::fs::remove_file(&provision_file);
            eprintln!("VM started");
        }
    });
    unsafe { machine.startWithOptions_completionHandler(&options, &started) };
    *VM.lock().unwrap() = Some(MainVm(machine));

    std::thread::spawn(move || serve_control(control));
    std::thread::spawn(move || serve_agent(agent));
    std::thread::spawn(move || wait_for_signals(signals));
    if display {
        show_window(mtm);
    }
    app.run();
    Ok(())
}

/// `setGuestProvisioningOptions:error:` (macOS 27) on the start options.
fn provision(options: &VZMacOSVirtualMachineStartOptions, p: &Provision) -> Result<()> {
    if AnyClass::get(c"VZMacGuestProvisioningOptions").is_none() {
        eprintln!("this macOS can't provision guests (needs macOS 27); finish Setup Assistant in the window: berth mac start --display");
        return Ok(());
    }
    unsafe {
        let prov = VZMacGuestProvisioningOptions::new();
        prov.setFullName(&NSString::from_str(&p.full_name));
        prov.setUsername(&NSString::from_str(&p.username));
        prov.setPassword(&NSString::from_str(&p.password));
        prov.setLogsInAutomatically(true);
        prov.setEnablesRemoteLogin(true);
        let result: Result<(), Retained<NSError>> = msg_send![options, setGuestProvisioningOptions: &*prov, error: _];
        result.map_err(|e| ns_error(&e)).context("guest provisioning options")
    }
}

fn stop_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

/// The first stop signal asks the guest to shut down; a second stops it.
fn wait_for_signals(set: libc::sigset_t) {
    let mut first = true;
    loop {
        let mut sig = 0;
        if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
            return;
        }
        eprintln!("signal {sig}: stopping the VM");
        let force = !first;
        first = false;
        let _ = on_main(move || stop(force));
    }
}

/// Ask the guest to shut down, stopping it hard after STOP_GRACE (or right
/// away with `force`, or if it can't be asked). Main queue only.
fn stop(force: bool) -> Result<(), String> {
    let guard = VM.lock().unwrap();
    let vm = &guard.as_ref().ok_or("no VM")?.0;
    if !force && unsafe { vm.canRequestStop() } {
        unsafe { vm.requestStopWithError() }.map_err(|e| e.localizedDescription().to_string())?;
        if !STOPPING.swap(true, Ordering::SeqCst) {
            std::thread::spawn(|| {
                std::thread::sleep(STOP_GRACE);
                eprintln!("the guest didn't shut down in {}s; stopping it", STOP_GRACE.as_secs());
                let _ = on_main(|| stop(true));
            });
        }
        return Ok(());
    }
    if !unsafe { vm.canStop() } {
        finish(0);
    }
    let done = RcBlock::new(|err: *mut NSError| {
        if let Some(err) = unsafe { err.as_ref() } {
            eprintln!("stopping the VM: {}", err.localizedDescription());
            finish(1);
        }
        eprintln!("VM stopped");
        finish(0);
    });
    unsafe { vm.stopWithCompletionHandler(&done) };
    Ok(())
}

fn state() -> &'static str {
    let guard = VM.lock().unwrap();
    match guard.as_ref().map(|vm| unsafe { vm.0.state() }) {
        Some(state) => crate::vm::state_name(state),
        None => "stopped",
    }
}

/// Open the window (or bring it back) and give the app a Dock icon.
/// Main thread only.
fn show_window(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    let shown = WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            window.makeKeyAndOrderFront(None);
            return true;
        }
        false
    });
    if !shown {
        let Some(vm) = VM.lock().unwrap().as_ref().map(|vm| vm.0.clone()) else { return };
        let window = make_window(mtm, &vm);
        window.makeKeyAndOrderFront(None);
        WINDOW.with(|w| *w.borrow_mut() = Some(window));
    }
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();
}

fn make_window(mtm: MainThreadMarker, vm: &VZVirtualMachine) -> Retained<NSWindow> {
    let d = DISPLAY.get().copied().unwrap_or_default();
    let scale = d.scale() as f64;
    let (mut w, mut h) = (d.width as f64 / scale, d.height as f64 / scale);
    // Fit the screen; the view scales the guest's display to the window.
    if let Some(screen) = NSScreen::mainScreen(mtm) {
        let frame = screen.visibleFrame();
        let fit = (frame.size.width * 0.9 / w).min(frame.size.height * 0.9 / h).min(1.0);
        (w, h) = (w * fit, h * fit);
    }
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
    unsafe {
        let view = VZVirtualMachineView::initWithFrame(VZVirtualMachineView::alloc(mtm), rect);
        view.setVirtualMachine(Some(vm));
        view.setCapturesSystemKeys(true);
        // A fixed guest display (see build_config), whatever the window size.
        view.setAutomaticallyReconfiguresDisplay(false);
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable;
        let window =
            NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false);
        // Closing hides it; the VM keeps running and `display` shows it again.
        window.setReleasedWhenClosed(false);
        let name = THIS.get().map_or("macOS", |vm| vm.name.as_str());
        window.setTitle(&NSString::from_str(&format!("{name} (berth mac)")));
        window.setContentView(Some(&view));
        window.setContentAspectRatio(NSSize::new(w, h));
        window.center();
        window
    }
}

/// `control.sock`: one JSON request line, one JSON reply line.
fn serve_control(listener: UnixListener) {
    for conn in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
            let mut line = String::new();
            if BufReader::new(&conn).read_line(&mut line).is_err() {
                return;
            }
            let reply = control_reply(&line);
            let _ = writeln!(&conn, "{reply}");
        });
    }
}

fn control_reply(line: &str) -> serde_json::Value {
    use serde_json::json;
    let req: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("bad request: {e}")}),
    };
    let result: Result<serde_json::Value, String> = match req["op"].as_str().unwrap_or_default() {
        "status" => on_main(|| {
            let window = WINDOW.with(|w| w.borrow().as_ref().is_some_and(|w| w.isVisible()));
            (state(), window)
        })
        .map(|(state, window)| {
            let vm = THIS.get();
            json!({
                "ok": true,
                "name": vm.map(|v| v.name.clone()),
                "state": state,
                "pid": std::process::id(),
                "started": STARTED.load(Ordering::SeqCst),
                "display": window,
                "agent": vm.map(|v| v.agent()),
            })
        })
        .map_err(|e| e.to_string()),
        "stop" => {
            let force = req["force"].as_bool().unwrap_or(false);
            on_main(move || stop(force)).map_err(|e| e.to_string()).and_then(|r| r).map(|()| json!({"ok": true}))
        }
        "display" => on_main(|| {
            if let Some(mtm) = MainThreadMarker::new() {
                show_window(mtm);
            }
        })
        .map(|()| json!({"ok": true}))
        .map_err(|e| e.to_string()),
        op => Err(format!("unknown op {op:?}; ops: status, stop, display")),
    };
    result.unwrap_or_else(|e| json!({"ok": false, "error": e}))
}

/// `agent.sock`: each connection goes to the guest agent's vsock port.
fn serve_agent(listener: UnixListener) {
    for conn in listener.incoming().flatten() {
        std::thread::spawn(move || match connect_vsock(GUEST_AGENT_PORT) {
            Ok(fd) => pump(conn, UnixStream::from(fd)),
            // The client expects one reply line; say why there isn't a guest.
            Err(e) => {
                let reply = serde_json::json!({"ok": false, "error": format!("the guest agent isn't reachable: {e}"), "payload_len": 0});
                let _ = writeln!(&conn, "{reply}");
            }
        });
    }
}

/// Copy both ways until each side has closed its half.
fn pump(a: UnixStream, b: UnixStream) {
    let (Ok(mut a2), Ok(mut b2)) = (a.try_clone(), b.try_clone()) else { return };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut a2, &mut b2);
        let _ = b2.shutdown(std::net::Shutdown::Write);
    });
    let (mut a, mut b) = (a, b);
    let _ = std::io::copy(&mut b, &mut a);
    let _ = a.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

/// As vm.rs's widen_buffers: vsock connections are Unix sockets to the VM
/// process with 8 KB buffers by default, too small for screenshots and pushes.
fn widen_buffers(fd: libc::c_int) {
    let size: libc::c_int = 4 << 20;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, opt, (&raw const size).cast(), size_of::<libc::c_int>() as libc::socklen_t);
        }
    }
}

/// A connection to `port` in the guest, as vm.rs's connect_vsock.
fn connect_vsock(port: u32) -> Result<OwnedFd, String> {
    let (tx, rx) = mpsc::channel::<Result<OwnedFd, String>>();
    DispatchQueue::main().exec_async(move || {
        let guard = VM.lock().unwrap();
        let device = guard
            .as_ref()
            .and_then(|vm| unsafe { vm.0.socketDevices() }.firstObject())
            .and_then(|d| d.downcast::<VZVirtioSocketDevice>().ok());
        let Some(device) = device else {
            let _ = tx.send(Err("no vsock device".into()));
            return;
        };
        let handler = RcBlock::new(move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
            let result = match (unsafe { conn.as_ref() }, unsafe { err.as_ref() }) {
                (Some(conn), None) => {
                    // Our own copy: VZ closes the connection's fd when it
                    // releases the object after this block.
                    let fd = unsafe { libc::dup(conn.fileDescriptor()) };
                    if fd < 0 {
                        Err(std::io::Error::last_os_error().to_string())
                    } else {
                        widen_buffers(fd);
                        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
                    }
                }
                (_, Some(err)) => Err(err.localizedDescription().to_string()),
                _ => Err("vsock connect returned nothing".into()),
            };
            let _ = tx.send(result);
        });
        unsafe { device.connectToPort_completionHandler(port, &handler) };
    });
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "timed out".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macvm::store::{Display, Meta};

    /// Builds and validates a whole configuration against the real
    /// framework, with the hardware model of the newest macOS Apple offers
    /// this Mac (metadata only, nothing downloaded) and an empty disk.
    /// Needs the network: `cargo test --release -p berth -- --ignored vz`.
    /// With BERTH_MAC_TEST_DIR set, the VM's directory is left there.
    #[test]
    #[ignore]
    fn vz_accepts_the_configuration() {
        let image = latest_restore_image().unwrap();
        let (model, min_cpus, min_mib) = image.requirements.expect("this Mac runs macOS VMs");
        let root = std::env::var("BERTH_MAC_TEST_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join(format!("berth-macvm-vz-{}", std::process::id())));
        let vm = Vm::new(&root, "gui-test-vz");
        let _ = std::fs::remove_dir_all(&vm.dir);
        std::fs::create_dir_all(&vm.dir).unwrap();
        let hw = new_hardware(&model);
        assert_eq!(hw.hardware_model_bytes().unwrap(), model);
        assert_ne!(new_hardware(&model).machine_identifier, hw.machine_identifier, "every VM gets its own identity");
        create_aux(&vm, &hw).unwrap();
        std::fs::File::create(vm.disk()).unwrap().set_len(1 << 30).unwrap();
        let meta = Meta {
            cpus: min_cpus.max(2),
            memory_mib: min_mib.max(4096),
            base: None,
            created: 0,
            user: Some("berth".into()),
            display: Display::default(),
            build: Some(image.build),
        };
        vm.save_hw(&hw).unwrap();
        vm.save_meta(&meta).unwrap();
        build_config(&vm, &meta, &hw).unwrap();
        assert!(vm.push().is_dir());
        // Provisioning options are accepted by the start options (macOS 27).
        if AnyClass::get(c"VZMacGuestProvisioningOptions").is_some() {
            let options = unsafe { VZMacOSVirtualMachineStartOptions::new() };
            let p = Provision { full_name: "Berth".into(), username: "berth".into(), password: "berth".into() };
            provision(&options, &p).unwrap();
        }
        if std::env::var_os("BERTH_MAC_TEST_DIR").is_none() {
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
