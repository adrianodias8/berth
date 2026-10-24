//! Virtualization.framework glue. The VM lives on the main dispatch queue, so
//! everything that touches it is funneled through `on_main`.

use std::fs;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{AllocAnyThread, define_class, msg_send};
use objc2_foundation::{NSArray, NSData, NSError, NSFileHandle, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_virtualization::*;
use tokio::sync::oneshot;

use crate::paths::Paths;

/// Guest vsock port of berth-agent (agent/src/main.rs).
pub const AGENT_VSOCK_PORT: u32 = 2400;
/// Host vsock port the agent connects to for guest-to-Mac traffic
/// (host.docker.internal); see daemon.rs.
pub const HOST_VSOCK_PORT: u32 = 2401;

/// Connections the guest opens to `HOST_VSOCK_PORT`, delivered to the daemon.
pub static INCOMING: OnceLock<tokio::sync::mpsc::UnboundedSender<OwnedFd>> = OnceLock::new();

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this type has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "BerthVsockListenerDelegate"]
    struct ListenerDelegate;

    unsafe impl NSObjectProtocol for ListenerDelegate {}

    unsafe impl VZVirtioSocketListenerDelegate for ListenerDelegate {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> Bool {
            // Dup as in connect_vsock: our copy outlives the connection object.
            let fd = unsafe { libc::dup(connection.fileDescriptor()) };
            if fd < 0 {
                return Bool::NO;
            }
            widen_buffers(fd);
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Bool::new(INCOMING.get().is_some_and(|tx| tx.send(fd).is_ok()))
        }
    }
);

impl ListenerDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// Listener and its delegate; the listener only holds the delegate weakly.
struct MainQueueListener(#[allow(dead_code)] Retained<VZVirtioSocketListener>, #[allow(dead_code)] Retained<ListenerDelegate>);
unsafe impl Send for MainQueueListener {}
static LISTENERS: Mutex<Vec<MainQueueListener>> = Mutex::new(Vec::new());

/// Accept guest-initiated vsock connections on `port`. Main thread only.
fn listen_vsock(vm: &VZVirtualMachine, port: u32) -> Result<()> {
    let devices = unsafe { vm.socketDevices() };
    let device = devices
        .firstObject()
        .and_then(|d| d.downcast::<VZVirtioSocketDevice>().ok())
        .context("no vsock device")?;
    let listener = unsafe { VZVirtioSocketListener::new() };
    let delegate = ListenerDelegate::new();
    unsafe {
        listener.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        device.setSocketListener_forPort(&listener, port);
    }
    LISTENERS.lock().unwrap().push(MainQueueListener(listener, delegate));
    Ok(())
}

/// The VM, touched only from blocks on the main queue. Not a thread_local:
/// after `dispatch_main()` the main queue is drained by a worker thread.
struct MainQueueVm(Retained<VZVirtualMachine>);
unsafe impl Send for MainQueueVm {}
static VM: Mutex<Option<MainQueueVm>> = Mutex::new(None);

pub struct Resources {
    pub cpus: usize,
    pub memory: u64,
}

impl Resources {
    pub fn default_for_host() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        let host_memory = host_memory_bytes();
        // Half of host RAM, capped at 16 GiB. The balloon device lets the guest
        // hand pages back, so this is a ceiling rather than a reservation.
        let memory = (host_memory / 2).clamp(2 << 30, 16 << 30);
        Self { cpus, memory }
    }

    /// Host defaults, overridden by `cpus` / `memory_mib` in ~/.berth/config.json
    /// (written by the app's settings). Values are clamped to what VZ allows.
    pub fn load(paths: &Paths) -> Self {
        #[derive(serde::Deserialize, Default)]
        struct Config {
            cpus: Option<usize>,
            memory_mib: Option<u64>,
        }
        let mut res = Self::default_for_host();
        let config: Config = std::fs::read(paths.config())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let (min_cpus, max_cpus) = unsafe {
            (
                VZVirtualMachineConfiguration::minimumAllowedCPUCount(),
                VZVirtualMachineConfiguration::maximumAllowedCPUCount(),
            )
        };
        let (min_mem, max_mem) = unsafe {
            (
                VZVirtualMachineConfiguration::minimumAllowedMemorySize(),
                VZVirtualMachineConfiguration::maximumAllowedMemorySize(),
            )
        };
        if let Some(cpus) = config.cpus {
            res.cpus = cpus.clamp(min_cpus.max(1), max_cpus);
        }
        if let Some(mib) = config.memory_mib {
            // VZ requires a multiple of 1 MiB; keep it within the host too.
            res.memory = mib.saturating_mul(1 << 20).clamp(min_mem.max(1 << 30), max_mem.min(host_memory_bytes()));
        }
        res
    }
}

fn host_memory_bytes() -> u64 {
    let mut value: u64 = 0;
    let mut len = size_of::<u64>();
    let name = c"hw.memsize";
    unsafe {
        libc::sysctlbyname(name.as_ptr(), (&raw mut value).cast(), &mut len, std::ptr::null_mut(), 0);
    }
    value
}

fn file_url(path: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn ns_error(err: &NSError) -> anyhow::Error {
    anyhow!("{}", err.localizedDescription())
}

/// Boot the guest's own kernel directly when the agent has exported it (see
/// agent/src/boot.rs), skipping EFI firmware and GRUB. A "trying" marker is
/// left for the agent to clear once up, counting the attempts that didn't get
/// there. One can be a stop before the guest was up; after two in a row,
/// direct boot is taken to fail, and we use EFI until a new kernel is exported.
fn direct_boot(paths: &Paths) -> Option<Retained<VZBootLoader>> {
    let dir = paths.guest().join("boot");
    let (kernel, initrd, trying, failed) =
        (dir.join("vmlinuz"), dir.join("initrd.img"), dir.join("trying"), dir.join("direct-failed"));
    // A safe boot needs our command line, so it boots directly whatever the
    // markers say (if it fails too, the next start uses EFI).
    let safe = safe_boot(paths);
    let attempts: u32 = fs::read_to_string(&trying).map_or(0, |s| s.trim().parse().unwrap_or(1));
    if attempts >= 2 && !safe {
        eprintln!("boot: last direct boot didn't come up; using EFI");
        let _ = fs::rename(&trying, &failed);
    }
    let cmdline = fs::read_to_string(dir.join("cmdline")).ok()?;
    if (failed.exists() && !safe) || !kernel.exists() || !initrd.exists() || cmdline.trim().is_empty() {
        return None;
    }
    let cmdline = if safe {
        // Mount home straight away (Apple's share, see virtiofs::home_share)
        // instead of through /etc/fstab, whatever it says; the agent then
        // repairs it (agent/src/kmod.rs).
        let home = paths.home.to_string_lossy();
        let args: Vec<&str> = cmdline.split_whitespace().map(|a| if a == "ro" { "rw" } else { a }).collect();
        format!("{} fstab=0 systemd.mount-extra=home:{home}:virtiofs:rw berth.safe=1", args.join(" "))
    } else {
        cmdline
    };
    fs::write(&trying, (attempts + 1).to_string()).ok()?;
    unsafe {
        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(&kernel));
        boot.setInitialRamdiskURL(Some(&file_url(&initrd)));
        boot.setCommandLine(&NSString::from_str(cmdline.trim()));
        Some(Retained::into_super(boot))
    }
}

/// The last start didn't come up (daemon.rs's watchdog): boot without the
/// guest's /etc/fstab, with home on Apple's share.
pub fn safe_boot(paths: &Paths) -> bool {
    paths.guest().join("boot/safe-boot").exists()
}

/// Build and validate the VM configuration. Must run on the main thread.
fn build_config(paths: &Paths, res: &Resources) -> Result<Retained<VZVirtualMachineConfiguration>> {
    unsafe {
        let config = VZVirtualMachineConfiguration::new();
        config.setCPUCount(res.cpus);
        config.setMemorySize(res.memory);

        // Platform with a stable machine identifier so the guest sees the same
        // hardware across boots.
        let platform = VZGenericPlatformConfiguration::new();
        let machine_id = fs::read(paths.machine_id())
            .ok()
            .and_then(|bytes| {
                VZGenericMachineIdentifier::initWithDataRepresentation(
                    VZGenericMachineIdentifier::alloc(),
                    &NSData::with_bytes(&bytes),
                )
            })
            .unwrap_or_else(|| {
                let id = VZGenericMachineIdentifier::new();
                fs::write(paths.machine_id(), id.dataRepresentation().to_vec()).ok();
                id
            });
        platform.setMachineIdentifier(&machine_id);
        config.setPlatform(&platform);

        match direct_boot(paths) {
            Some(boot) => config.setBootLoader(Some(&boot)),
            None => {
                // EFI boot through the image's own GRUB.
                let vars_url = file_url(&paths.efi_vars());
                let vars = if paths.efi_vars().exists() {
                    VZEFIVariableStore::initWithURL(VZEFIVariableStore::alloc(), &vars_url)
                } else {
                    VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
                        VZEFIVariableStore::alloc(),
                        &vars_url,
                        VZEFIVariableStoreInitializationOptions::empty(),
                    )
                    .map_err(|e| ns_error(&e))
                    .context("creating EFI variable store")?
                };
                let boot = VZEFIBootLoader::new();
                boot.setVariableStore(Some(&vars));
                config.setBootLoader(Some(&boot));
            }
        }

        // Root disk, then the cloud-init seed (read-only).
        let disk = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &file_url(&paths.disk()),
            false,
            VZDiskImageCachingMode::Cached,
            VZDiskImageSynchronizationMode::Fsync,
        )
        .map_err(|e| ns_error(&e))
        .context("attaching data disk")?;
        let seed = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &file_url(&paths.seed_iso()),
            true,
        )
        .map_err(|e| ns_error(&e))
        .context("attaching seed ISO")?;
        let storage = [disk, seed].map(|attachment| {
            Retained::into_super(VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &attachment,
            ))
        });
        config.setStorageDevices(&NSArray::from_retained_slice(&storage));

        // NAT networking with a persistent MAC so the DHCP lease stays put.
        let mac = fs::read_to_string(paths.mac_address())
            .ok()
            .and_then(|s| VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(s.trim())))
            .unwrap_or_else(|| {
                let mac = VZMACAddress::randomLocallyAdministeredAddress();
                fs::write(paths.mac_address(), mac.string().to_string()).ok();
                mac
            });
        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setMACAddress(&mac);
        net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        config.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));

        // Shared directories: the macOS home directory (berth's own device on
        // macOS 27, see virtiofs.rs; Apple's otherwise) and, if available, Rosetta.
        let home_share = crate::virtiofs::home_share(paths);
        let mut shares = Vec::new();
        if home_share == crate::virtiofs::HomeShare::Apple {
            let home = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &NSString::from_str("home"),
            );
            let home_dir = VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &file_url(&paths.home), false);
            let share = VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &home_dir);
            home.setShare(Some(&share));
            shares.push(Retained::into_super(home));
            for (tag, path) in crate::virtiofs::EXTRA_SHARES {
                let dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &NSString::from_str(tag),
                );
                let dir = VZSharedDirectory::initWithURL_readOnly(
                    VZSharedDirectory::alloc(),
                    &file_url(std::path::Path::new(path)),
                    false,
                );
                dev.setShare(Some(&VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir)));
                shares.push(Retained::into_super(dev));
            }
        }
        if VZLinuxRosettaDirectoryShare::availability() == VZLinuxRosettaAvailability::Installed {
            let rosetta = VZLinuxRosettaDirectoryShare::initWithError(VZLinuxRosettaDirectoryShare::alloc())
                .map_err(|e| ns_error(&e))
                .context("creating Rosetta share")?;
            let dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &NSString::from_str("rosetta"),
            );
            dev.setShare(Some(&rosetta));
            shares.push(Retained::into_super(dev));
        } else {
            eprintln!("Rosetta is not installed; x86-64 images will not run. Install it with `softwareupdate --install-rosetta`.");
        }
        config.setDirectorySharingDevices(&NSArray::from_retained_slice(&shares));
        crate::virtiofs::attach(&config, &paths.home, home_share);

        // Guest console goes to a log file for debugging.
        let console_fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        let log_path = std::ffi::CString::new(paths.console_log().to_string_lossy().as_bytes())?;
        let log_fd = libc::open(log_path.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, 0o644);
        if log_fd < 0 || console_fd < 0 {
            bail!("opening console log: {}", std::io::Error::last_os_error());
        }
        let serial_attachment = VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
            VZFileHandleSerialPortAttachment::alloc(),
            Some(&NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), console_fd, true)),
            Some(&NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), log_fd, true)),
        );
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(&serial_attachment));
        config.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(serial)]));

        config.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioSocketDeviceConfiguration::new(),
        )]));
        config.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioEntropyDeviceConfiguration::new(),
        )]));
        config.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new(),
        )]));

        config.validateWithError().map_err(|e| ns_error(&e)).context("invalid VM configuration")?;
        Ok(config)
    }
}

/// Create the VM and start it, or with `restore`, resume it from a saved
/// machine state (snapshots.rs), falling back to booting the disk if that
/// fails. Must run on the main thread before the main queue starts
/// draining; `on_started` is called on the main queue with whether the state
/// was restored.
pub fn create_and_start(
    paths: &Paths,
    res: &Resources,
    restore: Option<&std::path::Path>,
    on_started: impl Fn(Result<bool>) + 'static,
) -> Result<()> {
    let config = build_config(paths, res)?;
    let vm = unsafe { VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &config) };
    listen_vsock(&vm, HOST_VSOCK_PORT)?;
    let on_started = std::rc::Rc::new(on_started);
    let boot = {
        let on_started = on_started.clone();
        RcBlock::new(move |err: *mut NSError| {
            on_started(match unsafe { err.as_ref() } {
                None => Ok(false),
                Some(err) => Err(ns_error(err)),
            })
        })
    };
    match restore {
        None => unsafe { vm.startWithCompletionHandler(&boot) },
        Some(state) => {
            let trying = paths.guest().join("boot/trying");
            let vm2 = vm.clone();
            let restored = RcBlock::new(move |err: *mut NSError| {
                if let Some(err) = unsafe { err.as_ref() } {
                    eprintln!("snapshot: can't restore the saved state ({}); booting its disk instead", ns_error(err));
                    unsafe { vm2.startWithCompletionHandler(&boot) };
                    return;
                }
                // The restored guest is long past booting: nothing will clear
                // the direct-boot marker, and it would turn direct boot off.
                let _ = fs::remove_file(&trying);
                let on_started = on_started.clone();
                let resumed = RcBlock::new(move |err: *mut NSError| {
                    on_started(match unsafe { err.as_ref() } {
                        None => Ok(true),
                        Some(err) => Err(ns_error(err)),
                    })
                });
                unsafe { vm2.resumeWithCompletionHandler(&resumed) };
            });
            unsafe { vm.restoreMachineStateFromURL_completionHandler(&file_url(state), &restored) };
        }
    }
    *VM.lock().unwrap() = Some(MainQueueVm(vm));
    Ok(())
}

/// Run a VM operation that reports through a completion handler.
async fn with_completion(
    op: impl FnOnce(&VZVirtualMachine, &block2::DynBlock<dyn Fn(*mut NSError)>) + Send + 'static,
) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    let tx = Mutex::new(Some(tx));
    on_main(move |vm| {
        let handler = RcBlock::new(move |err: *mut NSError| {
            let result = unsafe { err.as_ref() }.map_or(Ok(()), |e| Err(ns_error(e).to_string()));
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(result);
            }
        });
        op(vm, &handler);
    })
    .await?;
    rx.await?.map_err(|e| anyhow!(e))
}

pub async fn pause() -> Result<()> {
    with_completion(|vm, done| unsafe { vm.pauseWithCompletionHandler(done) }).await
}

pub async fn resume() -> Result<()> {
    with_completion(|vm, done| unsafe { vm.resumeWithCompletionHandler(done) }).await
}

/// Save the paused VM's memory and device state to `path`. macOS encrypts
/// it with a key it can only create while the Mac is unlocked.
pub async fn save_state(path: std::path::PathBuf) -> Result<()> {
    with_completion(move |vm, done| unsafe { vm.saveMachineStateToURL_completionHandler(&file_url(&path), done) }).await
}

/// Run `f` with the VM on the main queue and return its result.
pub async fn on_main<T: Send + 'static>(f: impl FnOnce(&VZVirtualMachine) -> T + Send + 'static) -> Result<T> {
    let (tx, rx) = oneshot::channel();
    DispatchQueue::main().exec_async(move || {
        let result = VM.lock().unwrap().as_ref().map(|vm| f(&vm.0));
        let _ = tx.send(result);
    });
    rx.await?.context("VM not created")
}

pub fn state_name(state: VZVirtualMachineState) -> &'static str {
    match state {
        VZVirtualMachineState::Stopped => "stopped",
        VZVirtualMachineState::Running => "running",
        VZVirtualMachineState::Paused => "paused",
        VZVirtualMachineState::Error => "error",
        VZVirtualMachineState::Starting => "starting",
        VZVirtualMachineState::Stopping => "stopping",
        _ => "busy",
    }
}

pub async fn state() -> Result<VZVirtualMachineState> {
    on_main(|vm| unsafe { vm.state() }).await
}

/// Ask the guest to shut down (ACPI-style power button).
pub async fn request_stop() -> Result<()> {
    on_main(|vm| unsafe { vm.requestStopWithError() }.map_err(|e| ns_error(&e).to_string()))
        .await?
        .map_err(|e| anyhow!(e))
}

/// Hard stop, for when the guest ignores the stop request.
pub async fn force_stop() -> Result<()> {
    with_completion(|vm, done| unsafe { vm.stopWithCompletionHandler(done) }).await
}

fn balloon(vm: &VZVirtualMachine) -> Option<Retained<VZVirtioTraditionalMemoryBalloonDevice>> {
    unsafe { vm.memoryBalloonDevices() }
        .firstObject()
        .and_then(|d| d.downcast::<VZVirtioTraditionalMemoryBalloonDevice>().ok())
}

/// Let the guest hold `bytes` of memory; the rest goes back to macOS.
pub async fn set_balloon_target(bytes: u64) -> Result<()> {
    on_main(move |vm| balloon(vm).map(|b| unsafe { b.setTargetVirtualMachineMemorySize(bytes) }))
        .await?
        .context("no balloon device")
}

/// A vsock connection is a Unix socket to the VM process, and macOS gives
/// those 8 KB buffers: every large write went over in 8 KB pieces, which
/// capped Mac -> VM traffic at ~3 Gbit/s.
fn widen_buffers(fd: libc::c_int) {
    let size: libc::c_int = 4 << 20;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, opt, (&raw const size).cast(), size_of::<libc::c_int>() as libc::socklen_t);
        }
    }
}

/// Open a vsock connection to `port` in the guest. The returned fd is a
/// duplicate owned by the caller; the VZ connection object is released.
pub async fn connect_vsock(port: u32) -> Result<OwnedFd> {
    let (tx, rx) = oneshot::channel::<Result<OwnedFd, String>>();
    let tx = Mutex::new(Some(tx));
    on_main(move |vm| {
        let devices = unsafe { vm.socketDevices() };
        let Some(device) = devices.firstObject().and_then(|d| d.downcast::<VZVirtioSocketDevice>().ok()) else {
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(Err("no vsock device".into()));
            }
            return;
        };
        let handler = RcBlock::new(move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
            let result = match (unsafe { conn.as_ref() }, unsafe { err.as_ref() }) {
                (Some(conn), None) => {
                    // Dup rather than close: the connection object closes its
                    // own fd when VZ releases it after this block returns.
                    let fd = unsafe { libc::dup(conn.fileDescriptor()) };
                    widen_buffers(fd);
                    if fd < 0 {
                        Err(std::io::Error::last_os_error().to_string())
                    } else {
                        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
                    }
                }
                (_, Some(err)) => Err(ns_error(err).to_string()),
                _ => Err("vsock connect returned nothing".into()),
            };
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(result);
            }
        });
        unsafe { device.connectToPort_completionHandler(port, &handler) };
    })
    .await?;
    rx.await?.map_err(|e| anyhow!(e))
}
