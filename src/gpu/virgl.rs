//! Where 3D plugs in: virglrenderer (MIT) on ANGLE's EGL/GLES (BSD), whose
//! Metal backend runs it on the Mac's GPU. Built only with the `virgl`
//! cargo feature (off by default), which links libvirglrenderer and
//! ANGLE's libEGL; without it this file is `capsets()` returning nothing,
//! so the device offers no VIRGL feature and no capsets and the guest stays
//! on 2D. bench/spikes/gpu (S2) builds both libraries.
//!
//! How the pieces fit, once the 3D commands are routed here from server.rs:
//!
//! - **Start-up.** `Renderer::new` creates an ANGLE display on Metal
//!   (`eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE, …,
//!   EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE)`), then `virgl_renderer_init` with
//!   `USE_GLES | THREAD_SYNC` and callbacks of version 4, whose
//!   `get_egl_display` hands it that display and whose `create_gl_context`
//!   / `make_current` make its GLES contexts on ANGLE (not `USE_EGL`: that's
//!   virglrenderer's own EGL setup, which needs GBM, so macOS has none).
//! - **Capsets.** `virgl_renderer_get_cap_set` / `fill_caps` for VIRGL and
//!   VIRGL2 answer GET_CAPSET_INFO and GET_CAPSET (`capsets()`); the device
//!   then offers F_VIRGL (device.rs).
//! - **Commands.** CTX_CREATE/DESTROY → `context_create_with_flags` /
//!   `context_destroy`; CTX_ATTACH/DETACH_RESOURCE → `ctx_attach_resource` /
//!   `ctx_detach_resource`; RESOURCE_CREATE_3D → `resource_create`, its
//!   backing (the same scatter-gather entries the 2D server maps) passed as
//!   iovecs to `resource_attach_iov`; TRANSFER_TO/FROM_HOST_3D →
//!   `transfer_write_iov` / `transfer_read_iov`; SUBMIT_3D →
//!   `submit_cmd` (the stream in 32-bit words). A fenced command is
//!   answered when `write_fence` / `write_context_fence` reports it, after
//!   `virgl_renderer_poll` on the device queue.
//! - **Scanout.** A 3D resource on a scanout is a GL texture in ANGLE.
//!   ANGLE's `EGL_ANGLE_iosurface_client_buffer` wraps an IOSurface as a
//!   pbuffer; blitting the texture into it on RESOURCE_FLUSH gives an
//!   IOSurface that a CAMetalLayer shows without a copy, and frames.rs reads
//!   back for screenshots.
//! - **Blobs and Venus (games).** RESOURCE_CREATE_BLOB → `resource_create_blob`,
//!   RESOURCE_MAP_BLOB → `resource_map` and the shared memory region's
//!   `mapMemory:atOffset:size:` (spike S4) to put the host pages in the
//!   guest's view; Venus is the same API with the VENUS flag and a Vulkan
//!   driver (MoltenVK or KosmicKrisp) under virglrenderer.

use super::server::Capset;

/// What the renderer offers the guest; empty without the `virgl` feature.
pub fn capsets() -> Vec<Capset> {
    #[cfg(feature = "virgl")]
    {
        // Offering capsets makes Mesa go down the 3D path, which server.rs
        // doesn't route here yet: only for bringing that path up.
        if std::env::var_os("BERTH_GPU_VIRGL_CAPSETS").is_some() {
            return ffi::Renderer::get().map(|r| r.capsets()).unwrap_or_default();
        }
    }
    Vec::new()
}

#[cfg(feature = "virgl")]
pub mod ffi {
    //! virglrenderer.h and the EGL calls it needs, as of virglrenderer 1.1.

    #![allow(non_camel_case_types, dead_code)]

    use std::ffi::{c_char, c_int, c_void};
    use std::sync::OnceLock;

    use super::Capset;
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
    }

    const EGL_OPENGL_ES_API: u32 = 0x30A0;
    const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
    const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;

    /// The process's renderer (virglrenderer is one global per process).
    pub struct Renderer {
        display: usize,
    }

    unsafe extern "C" fn get_egl_display(cookie: *mut c_void) -> *mut c_void {
        cookie
    }

    // With get_egl_display, virglrenderer makes no contexts itself: these
    // make them on ANGLE's display, surfaceless and without a config
    // (EGL_KHR_surfaceless_context, EGL_KHR_no_config_context). Shared
    // ones share with the first, which is vrend's own.
    static FIRST: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

    unsafe extern "C" fn create_gl_context(cookie: *mut c_void, _scanout: c_int, param: *mut virgl_renderer_gl_ctx_param) -> virgl_renderer_gl_context {
        use std::sync::atomic::Ordering;
        let param = unsafe { &*param };
        let share = if param.shared { FIRST.load(Ordering::Acquire) } else { std::ptr::null_mut() };
        let attribs = [EGL_CONTEXT_MAJOR_VERSION, param.major_ver, EGL_CONTEXT_MINOR_VERSION, param.minor_ver, EGL_NONE as i32];
        let ctx = unsafe {
            eglBindAPI(EGL_OPENGL_ES_API);
            eglCreateContext(cookie, std::ptr::null_mut(), share, attribs.as_ptr())
        };
        // vrend asks for 4.6 first and steps down; ANGLE on Metal gives 3.0.
        if !ctx.is_null() {
            let _ = FIRST.compare_exchange(std::ptr::null_mut(), ctx, Ordering::AcqRel, Ordering::Acquire);
        }
        ctx
    }

    unsafe extern "C" fn destroy_gl_context(cookie: *mut c_void, ctx: virgl_renderer_gl_context) {
        unsafe { eglDestroyContext(cookie, ctx) };
    }

    unsafe extern "C" fn make_current(cookie: *mut c_void, _scanout: c_int, ctx: virgl_renderer_gl_context) -> c_int {
        let ok = unsafe { eglMakeCurrent(cookie, std::ptr::null_mut(), std::ptr::null_mut(), ctx) };
        if ok != 0 { 0 } else { -1 }
    }

    unsafe extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {}

    impl Renderer {
        /// Start it once; None (and logged) if ANGLE or virglrenderer fail.
        pub fn get() -> Option<&'static Renderer> {
            static R: OnceLock<Option<Renderer>> = OnceLock::new();
            R.get_or_init(|| unsafe {
                let attribs = [EGL_PLATFORM_ANGLE_TYPE_ANGLE, EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE, EGL_NONE];
                let display = eglGetPlatformDisplay(EGL_PLATFORM_ANGLE_ANGLE, std::ptr::null_mut(), attribs.as_ptr());
                if display.is_null() || eglInitialize(display, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                    eprintln!("gpu: ANGLE on Metal didn't start");
                    return None;
                }
                // virglrenderer keeps the pointer: it must live forever.
                let callbacks = Box::leak(Box::new(virgl_renderer_callbacks {
                    version: 4,
                    write_fence: Some(write_fence),
                    create_gl_context: Some(create_gl_context),
                    destroy_gl_context: Some(destroy_gl_context),
                    make_current: Some(make_current),
                    get_drm_fd: None,
                    write_context_fence: None,
                    get_server_fd: None,
                    get_egl_display: Some(get_egl_display),
                }));
                // Not USE_EGL: that's virglrenderer's own EGL setup, which on
                // macOS (no GBM) isn't built; the display comes from the callback.
                let flags = VIRGL_RENDERER_USE_GLES | VIRGL_RENDERER_THREAD_SYNC;
                let err = virgl_renderer_init(display, flags, callbacks);
                if err != 0 {
                    eprintln!("gpu: virglrenderer didn't start ({err})");
                    return None;
                }
                Some(Renderer { display: display as usize })
            })
            .as_ref()
        }

        /// VIRGL and VIRGL2, those the renderer has.
        pub fn capsets(&self) -> Vec<Capset> {
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
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(not(feature = "virgl"))]
    fn no_capsets_without_the_feature() {
        assert!(super::capsets().is_empty());
    }

    /// Spike S2: virglrenderer starts on ANGLE's Metal backend and offers
    /// its capsets (bench/spikes/gpu/s2-virgl-angle.sh runs it).
    #[test]
    #[cfg(feature = "virgl")]
    #[ignore = "needs ANGLE and virglrenderer built (bench/spikes/gpu, S2)"]
    fn probe_renderer() {
        let renderer = super::ffi::Renderer::get().expect("renderer didn't start");
        let caps = renderer.capsets();
        for c in &caps {
            println!("capset {} max_version {} size {}", c.id, c.max_version, c.data.len());
        }
        assert!(caps.iter().any(|c| c.id == crate::gpu::protocol::capset::VIRGL2 && c.max_version >= 1));
    }
}
