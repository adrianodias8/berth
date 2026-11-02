//! 3D: virglrenderer (MIT) on ANGLE's EGL/GLES (BSD), whose Metal backend
//! runs it on the Mac's GPU. Built only with the `virgl` cargo feature,
//! which links libvirglrenderer and ANGLE's libEGL; without it `backend`
//! is always None, so the device offers no VIRGL feature and no capsets and
//! the guest stays on 2D. bench/spikes/gpu (S2) builds both libraries.
//!
//! How the pieces fit:
//!
//! - **One thread.** ANGLE's contexts are current per OS thread, and
//!   virglrenderer only calls make_current when it switches contexts, so
//!   every EGL and virgl call runs on the berth-virgl worker
//!   (virgl_thread.rs), never on the device's dispatch queue (which has no
//!   fixed thread). `VirglBackend` is the server's Backend3d: each call is a
//!   job on the worker, answered synchronously.
//! - **Start-up** (on the worker). `Renderer::start` creates an ANGLE
//!   display on Metal (`eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE, …,
//!   EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE)`), then `virgl_renderer_init`
//!   with `USE_GLES | THREAD_SYNC` and callbacks of version 4, whose
//!   `get_egl_display` hands it that display and whose `create_gl_context`
//!   / `make_current` make its GLES contexts on ANGLE (not `USE_EGL`:
//!   that's virglrenderer's own EGL setup, which needs GBM, so macOS has
//!   none). THREAD_SYNC needs eventfd, so on macOS virglrenderer drops it:
//!   fences retire only when the worker polls.
//! - **Capsets.** `virgl_renderer_get_cap_set` / `fill_caps` for VIRGL and
//!   VIRGL2 answer GET_CAPSET_INFO and GET_CAPSET; the device then offers
//!   F_VIRGL (device.rs).
//! - **Commands** (routed by server.rs). CTX_CREATE/DESTROY →
//!   `context_create_with_flags` / `context_destroy`;
//!   CTX_ATTACH/DETACH_RESOURCE → `ctx_attach_resource` /
//!   `ctx_detach_resource`; RESOURCE_CREATE_3D → `resource_create`, its
//!   backing passed as iovecs to `resource_attach_iov`;
//!   TRANSFER_TO/FROM_HOST_3D → `transfer_write_iov` / `transfer_read_iov`;
//!   SUBMIT_3D → `submit_cmd` (the stream in 32-bit words). Fences →
//!   `create_fence` / `context_create_fence`, reported by `write_fence` /
//!   `write_context_fence` from `virgl_renderer_poll` on the worker.
//! - **Scanout.** A 3D resource on a scanout is a GL texture in ANGLE, read
//!   back with `transfer_read_iov` on SET_SCANOUT and RESOURCE_FLUSH
//!   (server.rs). Later, without a copy: ANGLE's
//!   `EGL_ANGLE_iosurface_client_buffer` wraps an IOSurface as a pbuffer,
//!   and blitting the texture into it gives an IOSurface a CAMetalLayer
//!   shows directly.
//! - **Blobs and Venus (games).** RESOURCE_CREATE_BLOB →
//!   `resource_create_blob`, RESOURCE_MAP_BLOB → `resource_map` and the
//!   shared memory region's `mapMemory:atOffset:size:` (spike S4) to put
//!   the host pages in the guest's view; Venus is the same API with the
//!   VENUS flag and a Vulkan driver (MoltenVK or KosmicKrisp) under
//!   virglrenderer. Off unless `gpu_blob`.
//!
//! Settings: `gpu_virgl` (default true) and `BERTH_GPU_VIRGL=0` turn it
//! off; a renderer that doesn't start leaves the 2D device (logged).

use super::Settings;
use super::backend::Backend3d;
use super::server::Capset;

/// The 3D renderer for the device, or None for a 2D-only device: built
/// without the feature, turned off, or the renderer didn't start.
pub fn backend(settings: &Settings) -> Option<Box<dyn Backend3d>> {
    if !settings.virgl {
        return None;
    }
    if std::env::var_os("BERTH_GPU_VIRGL").is_some_and(|v| v == "0") {
        eprintln!("gpu: 3D off (BERTH_GPU_VIRGL=0)");
        return None;
    }
    #[cfg(feature = "virgl")]
    {
        match VirglBackend::start() {
            Ok(mut b) => {
                let caps = b.capsets();
                if caps.is_empty() {
                    eprintln!("gpu: the 3D renderer offers no capsets; 2D only");
                    return None;
                }
                let names: Vec<_> = caps.iter().map(|c| format!("{} v{}", capset_name(c.id), c.max_version)).collect();
                eprintln!("gpu: 3D through virglrenderer on ANGLE (Metal), capsets {}", names.join(", "));
                return Some(Box::new(b));
            }
            Err(e) => {
                eprintln!("gpu: the 3D renderer didn't start ({e}); 2D only");
                return None;
            }
        }
    }
    #[allow(unreachable_code)]
    None
}

/// What the renderer offers, starting it if need be (on its own worker
/// thread): `berth gpu-probe`.
#[allow(dead_code)]
pub fn probe() -> Result<Vec<Capset>, String> {
    #[cfg(feature = "virgl")]
    {
        let worker = super::virgl_thread::VirglWorker::get()?;
        return worker.run(|_| ffi::capsets());
    }
    #[allow(unreachable_code)]
    Err("virgl: not built".into())
}

#[cfg(feature = "virgl")]
fn capset_name(id: u32) -> &'static str {
    use super::protocol::capset;
    match id {
        capset::VIRGL => "VIRGL",
        capset::VIRGL2 => "VIRGL2",
        capset::VENUS => "VENUS",
        _ => "other",
    }
}

#[cfg(feature = "virgl")]
pub use proxy::VirglBackend;

#[cfg(feature = "virgl")]
mod proxy {
    //! Backend3d over the worker: every call is a job on berth-virgl.

    use std::collections::HashMap;
    use std::ffi::{c_int, c_void};

    use super::ffi;
    use crate::gpu::backend::{Backend3d, BlobArgs, Iov, SignalSink};
    use crate::gpu::fence::Signal;
    use crate::gpu::protocol::{Create3d, Rect, Transfer3d};
    use crate::gpu::server::Capset;
    use crate::gpu::virgl_thread::VirglWorker;

    /// A pointer carried to the worker for one call, which the caller
    /// waits for, so what it points to outlives its use there.
    #[derive(Clone, Copy)]
    struct Ptr<T>(*mut T);
    unsafe impl<T> Send for Ptr<T> {}
    impl<T> Ptr<T> {
        fn get(self) -> *mut T {
            self.0
        }
    }

    pub struct VirglBackend {
        worker: &'static VirglWorker,
        /// virglrenderer keeps the iovec arrays it's given until the
        /// backing is detached or the resource goes: they live here.
        iovs: HashMap<u32, Vec<libc::iovec>>,
    }

    // SAFETY: the iovecs point into guest RAM the server keeps mapped while
    // they're attached; the worker is the only thread that uses them.
    unsafe impl Send for VirglBackend {}

    fn check(code: c_int) -> Result<(), i32> {
        if code == 0 { Ok(()) } else { Err(code) }
    }

    fn iovecs(iovs: &[Iov]) -> Vec<libc::iovec> {
        iovs.iter().map(|&(base, len)| libc::iovec { iov_base: base.cast(), iov_len: len }).collect()
    }

    impl VirglBackend {
        pub fn start() -> Result<Self, String> {
            Ok(Self { worker: VirglWorker::get()?, iovs: HashMap::new() })
        }

        /// A job on the worker; a worker that died fails every call.
        fn on<R: Send>(&self, f: impl FnOnce(&mut crate::gpu::virgl_thread::State) -> R + Send) -> Result<R, i32> {
            self.worker.run(f).map_err(|e| {
                eprintln!("gpu: {e}");
                libc::EIO
            })
        }
    }

    impl Backend3d for VirglBackend {
        fn capsets(&mut self) -> Vec<Capset> {
            self.on(|_| ffi::capsets()).unwrap_or_default()
        }

        fn ctx_create(&mut self, ctx: u32, capset_id: u32, name: &[u8]) -> Result<(), i32> {
            self.on(|_| unsafe {
                check(ffi::virgl_renderer_context_create_with_flags(ctx, capset_id, name.len() as u32, name.as_ptr().cast()))
            })?
        }

        fn ctx_destroy(&mut self, ctx: u32) {
            let _ = self.on(|s| {
                unsafe { ffi::virgl_renderer_context_destroy(ctx) };
                s.forget_context(ctx);
            });
        }

        fn ctx_attach_resource(&mut self, ctx: u32, res: u32) {
            let _ = self.on(|_| unsafe { ffi::virgl_renderer_ctx_attach_resource(ctx as c_int, res as c_int) });
        }

        fn ctx_detach_resource(&mut self, ctx: u32, res: u32) {
            let _ = self.on(|_| unsafe { ffi::virgl_renderer_ctx_detach_resource(ctx as c_int, res as c_int) });
        }

        fn resource_create_3d(&mut self, c: &Create3d) -> Result<(), i32> {
            let mut args = ffi::virgl_renderer_resource_create_args {
                handle: c.resource_id,
                target: c.target,
                format: c.format,
                bind: c.bind,
                width: c.width,
                height: c.height,
                depth: c.depth,
                array_size: c.array_size,
                last_level: c.last_level,
                nr_samples: c.nr_samples,
                flags: c.flags,
            };
            let args = Ptr(&mut args);
            self.on(move |_| unsafe { check(ffi::virgl_renderer_resource_create(args.get(), std::ptr::null_mut(), 0)) })?
        }

        fn resource_unref(&mut self, res: u32) {
            let _ = self.on(|_| unsafe { ffi::virgl_renderer_resource_unref(res) });
            self.iovs.remove(&res);
        }

        fn attach_iov(&mut self, res: u32, iovs: &[Iov]) -> Result<(), i32> {
            let mut v = iovecs(iovs);
            let (p, n) = (Ptr(v.as_mut_ptr()), v.len() as c_int);
            self.on(move |_| unsafe { check(ffi::virgl_renderer_resource_attach_iov(res as c_int, p.get(), n)) })??;
            // Moving the Vec keeps its buffer where virglrenderer has it.
            self.iovs.insert(res, v);
            Ok(())
        }

        fn detach_iov(&mut self, res: u32) {
            let _ = self.on(|_| unsafe {
                let (mut iov, mut n) = (std::ptr::null_mut(), 0);
                ffi::virgl_renderer_resource_detach_iov(res as c_int, &mut iov, &mut n);
            });
            self.iovs.remove(&res);
        }

        fn transfer_to_host_3d(&mut self, ctx: u32, t: &Transfer3d) -> Result<(), i32> {
            let t = *t;
            self.on(move |_| unsafe {
                let mut b = ffi::virgl_box { x: t.box_.x, y: t.box_.y, z: t.box_.z, w: t.box_.w, h: t.box_.h, d: t.box_.d };
                // No iovecs: the resource's attached backing.
                check(ffi::virgl_renderer_transfer_write_iov(
                    t.resource_id,
                    ctx,
                    t.level as c_int,
                    t.stride,
                    t.layer_stride,
                    &mut b,
                    t.offset,
                    std::ptr::null_mut(),
                    0,
                ))
            })?
        }

        fn transfer_from_host_3d(&mut self, ctx: u32, t: &Transfer3d) -> Result<(), i32> {
            let t = *t;
            self.on(move |_| unsafe {
                let mut b = ffi::virgl_box { x: t.box_.x, y: t.box_.y, z: t.box_.z, w: t.box_.w, h: t.box_.h, d: t.box_.d };
                check(ffi::virgl_renderer_transfer_read_iov(
                    t.resource_id,
                    ctx,
                    t.level,
                    t.stride,
                    t.layer_stride,
                    &mut b,
                    t.offset,
                    std::ptr::null_mut(),
                    0,
                ))
            })?
        }

        fn submit_3d(&mut self, ctx: u32, commands: &[u8]) -> Result<(), i32> {
            self.on(|_| {
                // virglrenderer reads 32-bit words: copy into aligned ones.
                let mut words: Vec<u32> = commands.as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w)).collect();
                unsafe { check(ffi::virgl_renderer_submit_cmd(words.as_mut_ptr().cast(), ctx as c_int, words.len() as c_int)) }
            })?
        }

        fn create_fence(&mut self, host_seq: u32) -> Result<(), i32> {
            self.on(|s| {
                // The context argument is unused: these are ctx 0 fences.
                check(unsafe { ffi::virgl_renderer_create_fence(host_seq as c_int, 0) })?;
                s.fence_created(Signal::Global(host_seq));
                Ok(())
            })?
        }

        fn create_context_fence(&mut self, ctx: u32, ring: u32, fence: u64) -> Result<(), i32> {
            self.on(|s| {
                check(unsafe { ffi::virgl_renderer_context_create_fence(ctx, 0, ring, fence) })?;
                s.fence_created(Signal::Ring { ctx, ring, fence });
                Ok(())
            })?
        }

        fn set_signal_sink(&mut self, sink: SignalSink) {
            let _ = self.on(|s| s.sink = Some(sink));
        }

        fn read_back(&mut self, res: u32, rect: Rect, dst: &mut [u8], stride: usize) -> Result<(), i32> {
            let (p, len) = (Ptr(dst.as_mut_ptr()), dst.len());
            self.on(move |_| unsafe {
                let mut iov = libc::iovec { iov_base: p.get().cast(), iov_len: len };
                let mut b = ffi::virgl_box { x: rect.x, y: rect.y, z: 0, w: rect.width, h: rect.height, d: 1 };
                check(ffi::virgl_renderer_transfer_read_iov(res, 0, 0, stride as u32, 0, &mut b, 0, &mut iov, 1))
            })?
        }

        fn resource_create_blob(&mut self, a: &BlobArgs, iovs: &[Iov]) -> Result<(), i32> {
            let mut v = iovecs(iovs);
            let (p, n) = (Ptr(v.as_mut_ptr()), v.len() as u32);
            let a = *a;
            self.on(move |_| unsafe {
                let args = ffi::virgl_renderer_resource_create_blob_args {
                    res_handle: a.resource_id,
                    ctx_id: a.ctx_id,
                    blob_mem: a.blob_mem,
                    blob_flags: a.blob_flags,
                    blob_id: a.blob_id,
                    size: a.size,
                    iovecs: if n == 0 { std::ptr::null() } else { p.get() },
                    num_iovs: n,
                };
                check(ffi::virgl_renderer_resource_create_blob(&args))
            })??;
            if !v.is_empty() {
                self.iovs.insert(a.resource_id, v);
            }
            Ok(())
        }

        fn resource_map(&mut self, res: u32) -> Result<(*mut u8, u64), i32> {
            let (addr, size) = self.on(|_| unsafe {
                let (mut map, mut size) = (std::ptr::null_mut::<c_void>(), 0u64);
                check(ffi::virgl_renderer_resource_map(res, &mut map, &mut size)).map(|()| (map as usize, size))
            })??;
            Ok((addr as *mut u8, size))
        }

        fn resource_unmap(&mut self, res: u32) -> Result<(), i32> {
            self.on(|_| unsafe { check(ffi::virgl_renderer_resource_unmap(res)) })?
        }

        fn map_info(&mut self, res: u32) -> Result<u32, i32> {
            self.on(|_| unsafe {
                let mut info = 0u32;
                check(ffi::virgl_renderer_resource_get_map_info(res, &mut info)).map(|()| info)
            })?
        }
    }
}

#[cfg(feature = "virgl")]
pub mod ffi {
    //! virglrenderer.h and the EGL calls it needs, as of virglrenderer 1.1.
    //! Only ever called on the berth-virgl worker (virgl_thread.rs).

    #![allow(non_camel_case_types, dead_code)]

    use std::ffi::{c_char, c_int, c_void};

    use super::Capset;
    use crate::gpu::fence::Signal;
    use crate::gpu::protocol::capset;

    pub const VIRGL_RENDERER_USE_EGL: c_int = 1 << 0;
    pub const VIRGL_RENDERER_THREAD_SYNC: c_int = 1 << 1;
    pub const VIRGL_RENDERER_USE_SURFACELESS: c_int = 1 << 3;
    pub const VIRGL_RENDERER_USE_GLES: c_int = 1 << 4;
    pub const VIRGL_RENDERER_USE_EXTERNAL_BLOB: c_int = 1 << 5;
    pub const VIRGL_RENDERER_VENUS: c_int = 1 << 6;

    pub type virgl_renderer_gl_context = *mut c_void;

    #[repr(C)]
    pub struct virgl_renderer_gl_ctx_param {
        pub version: c_int,
        pub shared: bool,
        pub major_ver: c_int,
        pub minor_ver: c_int,
        pub compat_ctx: c_int,
    }

    #[repr(C)]
    pub struct virgl_renderer_callbacks {
        pub version: c_int,
        pub write_fence: Option<unsafe extern "C" fn(cookie: *mut c_void, fence: u32)>,
        pub create_gl_context: Option<
            unsafe extern "C" fn(cookie: *mut c_void, scanout: c_int, param: *mut virgl_renderer_gl_ctx_param) -> virgl_renderer_gl_context,
        >,
        pub destroy_gl_context: Option<unsafe extern "C" fn(cookie: *mut c_void, ctx: virgl_renderer_gl_context)>,
        pub make_current: Option<unsafe extern "C" fn(cookie: *mut c_void, scanout: c_int, ctx: virgl_renderer_gl_context) -> c_int>,
        pub get_drm_fd: Option<unsafe extern "C" fn(cookie: *mut c_void) -> c_int>,
        pub write_context_fence: Option<unsafe extern "C" fn(cookie: *mut c_void, ctx_id: u32, ring_idx: u32, fence_id: u64)>,
        pub get_server_fd: Option<unsafe extern "C" fn(cookie: *mut c_void, version: u32) -> c_int>,
        pub get_egl_display: Option<unsafe extern "C" fn(cookie: *mut c_void) -> *mut c_void>,
    }

    #[repr(C)]
    pub struct virgl_renderer_resource_create_args {
        pub handle: u32,
        pub target: u32,
        pub format: u32,
        pub bind: u32,
        pub width: u32,
        pub height: u32,
        pub depth: u32,
        pub array_size: u32,
        pub last_level: u32,
        pub nr_samples: u32,
        pub flags: u32,
    }

    #[repr(C)]
    pub struct virgl_box {
        pub x: u32,
        pub y: u32,
        pub z: u32,
        pub w: u32,
        pub h: u32,
        pub d: u32,
    }

    #[repr(C)]
    pub struct virgl_renderer_resource_create_blob_args {
        pub res_handle: u32,
        pub ctx_id: u32,
        pub blob_mem: u32,
        pub blob_flags: u32,
        pub blob_id: u64,
        pub size: u64,
        pub iovecs: *const libc::iovec,
        pub num_iovs: u32,
    }

    #[link(name = "virglrenderer")]
    unsafe extern "C" {
        pub fn virgl_renderer_init(cookie: *mut c_void, flags: c_int, cb: *mut virgl_renderer_callbacks) -> c_int;
        pub fn virgl_renderer_cleanup(cookie: *mut c_void);
        pub fn virgl_renderer_poll();
        pub fn virgl_renderer_get_poll_fd() -> c_int;
        pub fn virgl_renderer_get_cap_set(set: u32, max_ver: *mut u32, max_size: *mut u32);
        pub fn virgl_renderer_fill_caps(set: u32, version: u32, caps: *mut c_void);
        pub fn virgl_renderer_context_create_with_flags(ctx_id: u32, ctx_flags: u32, nlen: u32, name: *const c_char) -> c_int;
        pub fn virgl_renderer_context_destroy(handle: u32);
        pub fn virgl_renderer_submit_cmd(buffer: *mut c_void, ctx_id: c_int, ndw: c_int) -> c_int;
        pub fn virgl_renderer_create_fence(client_fence_id: c_int, ctx_id: u32) -> c_int;
        pub fn virgl_renderer_context_create_fence(ctx_id: u32, flags: u32, ring_idx: u32, fence_id: u64) -> c_int;
        pub fn virgl_renderer_resource_create(args: *mut virgl_renderer_resource_create_args, iov: *mut libc::iovec, num_iovs: u32) -> c_int;
        pub fn virgl_renderer_resource_unref(res_handle: u32);
        pub fn virgl_renderer_resource_attach_iov(res_handle: c_int, iov: *mut libc::iovec, num_iovs: c_int) -> c_int;
        pub fn virgl_renderer_resource_detach_iov(res_handle: c_int, iov: *mut *mut libc::iovec, num_iovs: *mut c_int);
        pub fn virgl_renderer_ctx_attach_resource(ctx_id: c_int, res_handle: c_int);
        pub fn virgl_renderer_ctx_detach_resource(ctx_id: c_int, res_handle: c_int);
        pub fn virgl_renderer_transfer_write_iov(
            handle: u32,
            ctx_id: u32,
            level: c_int,
            stride: u32,
            layer_stride: u32,
            box_: *mut virgl_box,
            offset: u64,
            iovec: *mut libc::iovec,
            iovec_cnt: u32,
        ) -> c_int;
        pub fn virgl_renderer_transfer_read_iov(
            handle: u32,
            ctx_id: u32,
            level: u32,
            stride: u32,
            layer_stride: u32,
            box_: *mut virgl_box,
            offset: u64,
            iov: *mut libc::iovec,
            iovec_cnt: c_int,
        ) -> c_int;
        pub fn virgl_renderer_resource_create_blob(args: *const virgl_renderer_resource_create_blob_args) -> c_int;
        pub fn virgl_renderer_resource_map(res_handle: u32, map: *mut *mut c_void, out_size: *mut u64) -> c_int;
        pub fn virgl_renderer_resource_unmap(res_handle: u32) -> c_int;
        pub fn virgl_renderer_resource_get_map_info(res_handle: u32, map_info: *mut u32) -> c_int;
    }

    pub const EGL_NONE: isize = 0x3038;
    pub const EGL_PLATFORM_ANGLE_ANGLE: u32 = 0x3202;
    pub const EGL_PLATFORM_ANGLE_TYPE_ANGLE: isize = 0x3203;
    pub const EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE: isize = 0x3489;

    // ANGLE's libEGL.
    #[link(name = "EGL")]
    unsafe extern "C" {
        pub fn eglGetPlatformDisplay(platform: u32, native_display: *mut c_void, attribs: *const isize) -> *mut c_void;
        pub fn eglInitialize(display: *mut c_void, major: *mut i32, minor: *mut i32) -> u32;
        pub fn eglBindAPI(api: u32) -> u32;
        pub fn eglCreateContext(display: *mut c_void, config: *mut c_void, share: *mut c_void, attribs: *const i32) -> *mut c_void;
        pub fn eglDestroyContext(display: *mut c_void, context: *mut c_void) -> u32;
        pub fn eglMakeCurrent(display: *mut c_void, draw: *mut c_void, read: *mut c_void, context: *mut c_void) -> u32;
        pub fn eglGetCurrentContext() -> *mut c_void;
    }


    const EGL_OPENGL_ES_API: u32 = 0x30A0;
    const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
    const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;

    /// The process's renderer (virglrenderer is one global per process).
    /// Its methods run on the berth-virgl worker, from any thread.
    pub struct Renderer {
        _private: (),
    }

    static RENDERER: Renderer = Renderer { _private: () };

    unsafe extern "C" fn get_egl_display(cookie: *mut c_void) -> *mut c_void {
        cookie
    }

    // With get_egl_display, virglrenderer makes no contexts itself: these
    // make them on ANGLE's display, surfaceless and without a config
    // (EGL_KHR_surfaceless_context, EGL_KHR_no_config_context). Shared
    // ones share with the current one, as virglrenderer's own EGL setup
    // does: vrend makes its first contexts while probing GL versions and
    // destroys them, so the first isn't one to share with.
    unsafe extern "C" fn create_gl_context(cookie: *mut c_void, _scanout: c_int, param: *mut virgl_renderer_gl_ctx_param) -> virgl_renderer_gl_context {
        let param = unsafe { &*param };
        let share = if param.shared { unsafe { eglGetCurrentContext() } } else { std::ptr::null_mut() };
        let attribs = [EGL_CONTEXT_MAJOR_VERSION, param.major_ver, EGL_CONTEXT_MINOR_VERSION, param.minor_ver, EGL_NONE as i32];
        // vrend asks for 4.6 first and steps down; ANGLE on Metal gives 3.0.
        unsafe {
            eglBindAPI(EGL_OPENGL_ES_API);
            eglCreateContext(cookie, std::ptr::null_mut(), share, attribs.as_ptr())
        }
    }

    unsafe extern "C" fn destroy_gl_context(cookie: *mut c_void, ctx: virgl_renderer_gl_context) {
        unsafe { eglDestroyContext(cookie, ctx) };
    }

    unsafe extern "C" fn make_current(cookie: *mut c_void, _scanout: c_int, ctx: virgl_renderer_gl_context) -> c_int {
        let ok = unsafe { eglMakeCurrent(cookie, std::ptr::null_mut(), std::ptr::null_mut(), ctx) };
        if ok != 0 { 0 } else { -1 }
    }

    // Fences retire inside virgl_renderer_poll, on the worker.
    unsafe extern "C" fn write_fence(_cookie: *mut c_void, fence: u32) {
        crate::gpu::virgl_thread::signalled(Signal::Global(fence));
    }

    unsafe extern "C" fn write_context_fence(_cookie: *mut c_void, ctx: u32, ring: u32, fence: u64) {
        crate::gpu::virgl_thread::signalled(Signal::Ring { ctx, ring, fence });
    }

    impl Renderer {
        /// Start ANGLE and virglrenderer. Once, on the worker thread: the
        /// thread that does is where vrend's contexts are current.
        pub(in crate::gpu) fn start() -> Result<(), String> {
            unsafe {
                let attribs = [EGL_PLATFORM_ANGLE_TYPE_ANGLE, EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE, EGL_NONE];
                let display = eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE, std::ptr::null_mut(), attribs.as_ptr());
                if display.is_null() || eglInitialize(display, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                    return Err("ANGLE on Metal didn't start".into());
                }
                // virglrenderer keeps the pointer: it must live forever.
                let callbacks = Box::leak(Box::new(virgl_renderer_callbacks {
                    version: 4,
                    write_fence: Some(write_fence),
                    create_gl_context: Some(create_gl_context),
                    destroy_gl_context: Some(destroy_gl_context),
                    make_current: Some(make_current),
                    get_drm_fd: None,
                    write_context_fence: Some(write_context_fence),
                    get_server_fd: None,
                    get_egl_display: Some(get_egl_display),
                }));
                // Not USE_EGL: that's virglrenderer's own EGL setup, which on
                // macOS (no GBM) isn't built; the display comes from the callback.
                let flags = VIRGL_RENDERER_USE_GLES | VIRGL_RENDERER_THREAD_SYNC;
                let err = virgl_renderer_init(display, flags, callbacks);
                if err != 0 {
                    return Err(format!("virglrenderer didn't start ({err})"));
                }
            }
            Ok(())
        }

        /// The renderer, started on its worker if it isn't yet; None (and
        /// logged) if ANGLE or virglrenderer fail.
        pub fn get() -> Option<&'static Renderer> {
            match crate::gpu::virgl_thread::VirglWorker::get() {
                Ok(_) => Some(&RENDERER),
                Err(e) => {
                    eprintln!("gpu: {e}");
                    None
                }
            }
        }

        /// VIRGL and VIRGL2, those the renderer has.
        pub fn capsets(&self) -> Vec<Capset> {
            crate::gpu::virgl_thread::VirglWorker::get().and_then(|w| w.run(|_| capsets())).unwrap_or_default()
        }
    }

    /// VIRGL and VIRGL2, those the renderer has. Worker thread only.
    pub(in crate::gpu) fn capsets() -> Vec<Capset> {
        [capset::VIRGL, capset::VIRGL2]
            .into_iter()
            .filter_map(|id| unsafe {
                let (mut version, mut size) = (0u32, 0u32);
                virgl_renderer_get_cap_set(id, &mut version, &mut size);
                if version == 0 || size == 0 {
                    return None;
                }
                let mut data = vec![0u8; size as usize];
                virgl_renderer_fill_caps(id, version, data.as_mut_ptr().cast());
                Some(Capset { id, max_version: version, data })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(not(feature = "virgl"))]
    fn no_renderer_without_the_feature() {
        let settings = crate::gpu::Settings { gui: true, memory_mib: 256, display: (1920, 1080), virgl: true, blob: false };
        assert!(super::backend(&settings).is_none());
        assert_eq!(super::probe().unwrap_err(), "virgl: not built");
    }

    #[test]
    fn turned_off_means_no_renderer() {
        let settings = crate::gpu::Settings { gui: true, memory_mib: 256, display: (1920, 1080), virgl: false, blob: false };
        assert!(super::backend(&settings).is_none());
    }

    /// The tests below need ANGLE and virglrenderer built (bench/spikes/gpu,
    /// S2); they share the process's one renderer.
    #[cfg(feature = "virgl")]
    mod live {
        use std::sync::mpsc;
        use std::time::Duration;

        use crate::gpu::backend::Backend3d;
        use crate::gpu::fence::Signal;
        use crate::gpu::protocol::{Box3d, Create3d, Rect, Transfer3d, capset};
        use crate::gpu::virgl::VirglBackend;

        const B8G8R8A8: u32 = 1;
        const TEXTURE_2D: u32 = 2;
        const BIND_RENDER_TARGET: u32 = 1 << 1;
        const BIND_SAMPLER_VIEW: u32 = 1 << 3;

        /// One renderer and one signal sink per process: one test at a time.
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

        /// A backend whose fence signals arrive on the returned channel.
        fn backend() -> (VirglBackend, mpsc::Receiver<Vec<Signal>>) {
            let mut b = VirglBackend::start().expect("renderer didn't start");
            let (tx, rx) = mpsc::channel();
            b.set_signal_sink(Box::new(move |s| {
                let _ = tx.send(s);
            }));
            (b, rx)
        }

        /// Wait for the global fence `seq`.
        fn wait(rx: &mpsc::Receiver<Vec<Signal>>, seq: u32) {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                let signals = rx.recv_timeout(left).unwrap_or_else(|_| panic!("fence {seq} not signalled within 1 s"));
                if signals.iter().any(|s| matches!(s, Signal::Global(n) if *n >= seq)) {
                    return;
                }
            }
        }

        fn texture(id: u32) -> Create3d {
            Create3d {
                resource_id: id,
                target: TEXTURE_2D,
                format: B8G8R8A8,
                bind: BIND_RENDER_TARGET | BIND_SAMPLER_VIEW,
                width: 64,
                height: 64,
                depth: 1,
                array_size: 1,
                flags: 1, // Y_0_TOP
                ..Default::default()
            }
        }

        /// Spike S2: virglrenderer starts on ANGLE's Metal backend and offers
        /// its capsets (bench/spikes/gpu/s2-virgl-angle.sh runs it).
        #[test]
        #[ignore = "needs ANGLE and virglrenderer built (bench/spikes/gpu, S2)"]
        fn probe_renderer() {
            let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let caps = super::super::probe().expect("renderer didn't start");
            for c in &caps {
                println!("capset {} max_version {} size {}", c.id, c.max_version, c.data.len());
            }
            assert!(caps.iter().any(|c| c.id == capset::VIRGL2 && c.max_version >= 1));
        }

        /// A pattern goes up into a texture and comes back unchanged, with a
        /// fenced (empty) submit between, signalled within a second.
        #[test]
        #[ignore = "needs ANGLE and virglrenderer built (bench/spikes/gpu, S2)"]
        fn virgl_roundtrip() {
            let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let (mut b, rx) = backend();
            let (ctx, res) = (101, 1001);
            b.ctx_create(ctx, capset::VIRGL2, b"roundtrip").unwrap();
            b.resource_create_3d(&texture(res)).unwrap();
            b.ctx_attach_resource(ctx, res);
            let mut backing: Vec<u8> = (0..64 * 64 * 4).map(|i| (i * 7 % 251) as u8).collect();
            let pattern = backing.clone();
            b.attach_iov(res, &[(backing.as_mut_ptr(), backing.len())]).unwrap();
            let t = Transfer3d { box_: Box3d { x: 0, y: 0, z: 0, w: 64, h: 64, d: 1 }, resource_id: res, stride: 256, ..Default::default() };
            b.transfer_to_host_3d(ctx, &t).unwrap();
            b.submit_3d(ctx, &[]).unwrap();
            b.create_fence(7001).unwrap();
            let fenced = std::time::Instant::now();
            wait(&rx, 7001);
            println!("fence signalled in {:?}", fenced.elapsed());
            backing.fill(0);
            b.transfer_from_host_3d(ctx, &t).unwrap();
            assert!(backing == pattern, "the texture came back changed");
            b.detach_iov(res);
            b.resource_unref(res);
            b.ctx_destroy(ctx);
        }

        /// A real draw: clear a render target to red, then read it back.
        #[test]
        #[ignore = "needs ANGLE and virglrenderer built (bench/spikes/gpu, S2)"]
        fn virgl_clear_reads_back_red() {
            let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let (mut b, rx) = backend();
            let (ctx, res, surface) = (102, 1002, 1);
            b.ctx_create(ctx, capset::VIRGL2, b"clear").unwrap();
            b.resource_create_3d(&texture(res)).unwrap();
            b.ctx_attach_resource(ctx, res);
            // virgl_protocol.h: VIRGL_CMD0(cmd, object, length).
            let cmd0 = |cmd: u32, obj: u32, len: u32| cmd | obj << 8 | len << 16;
            let (create_object, set_framebuffer_state, clear, object_surface) = (1, 5, 7, 8);
            let clear_color0 = 1 << 2;
            let words = [
                cmd0(create_object, object_surface, 5),
                surface,
                res,
                B8G8R8A8,
                0, // level
                0, // first and last layer
                cmd0(set_framebuffer_state, 0, 3),
                1, // one colour buffer
                0, // no depth
                surface,
                cmd0(clear, 0, 8),
                clear_color0,
                1.0f32.to_bits(),
                0.0f32.to_bits(),
                0.0f32.to_bits(),
                1.0f32.to_bits(),
                0, // depth, a double
                0,
                0, // stencil
            ];
            let stream: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            b.submit_3d(ctx, &stream).unwrap();
            b.create_fence(7002).unwrap();
            wait(&rx, 7002);
            let mut px = vec![0u8; 64 * 64 * 4];
            b.read_back(res, Rect::new(0, 0, 64, 64), &mut px, 256).unwrap();
            assert!(px.chunks(4).all(|p| p == [0, 0, 0xff, 0xff]), "first pixels {:?}", &px[..8]);
            b.resource_unref(res);
            b.ctx_destroy(ctx);
        }

        /// Calls from any thread work: the worker is where GL runs.
        #[test]
        #[ignore = "needs ANGLE and virglrenderer built (bench/spikes/gpu, S2)"]
        fn calls_from_another_thread() {
            let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let (mut b, rx) = backend();
            b.ctx_create(103, capset::VIRGL2, b"here").unwrap();
            let other = std::thread::spawn(move || {
                b.resource_create_3d(&texture(1003)).unwrap();
                b.ctx_attach_resource(103, 1003);
                b.submit_3d(103, &[]).unwrap();
                b.create_fence(7003).unwrap();
                wait(&rx, 7003);
                let mut px = vec![0u8; 16 * 4];
                b.read_back(1003, Rect::new(0, 0, 16, 1), &mut px, 64).unwrap();
                b.resource_unref(1003);
                b.ctx_destroy(103);
                assert!(!b.capsets().is_empty());
            });
            other.join().unwrap();
        }
    }
}
