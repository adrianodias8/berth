//! Scanout readback without the CPU detiling (docs/LEARNINGS.md, Phase 3).
//!
//! virglrenderer's `transfer_read_iov` is a `glReadPixels` into client
//! memory, which ANGLE on Metal serves with `-[MTLTexture getBytes:]`: the
//! CPU decompresses the GPU's lossless-compressed texture, a 1080p frame
//! taking most of the worker's time and holding up the guest's commands
//! behind it. Read into a pixel pack buffer instead and ANGLE copies on the
//! GPU; mapping the buffer is then a plain copy.
//!
//! It runs on the worker in a context of its own, in vrend's share group
//! (the resource's GL texture is vrend's), so none of vrend's GL state is
//! touched, and the context vrend had current is current again after. It
//! gives the same bytes `transfer_read_iov` does for what it takes: 2D
//! RGBA/BGRA resources without Y_0_TOP (stored RGBA on GLES, BGR* swizzled
//! back, as vrend does). Anything else, or a GL error, is `None`: the
//! caller falls back to `transfer_read_iov`.

use std::cell::RefCell;
use std::ffi::{CStr, c_int, c_void};

use crate::gpu::protocol::{Format, Rect};

const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_PIXEL_PACK_BUFFER: u32 = 0x88EB;
const GL_STREAM_READ: u32 = 0x88E1;
const GL_MAP_READ_BIT: u32 = 0x0001;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_NO_ERROR: u32 = 0;

const EGL_NONE: i32 = 0x3038;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
/// virglrenderer.h: the resource's rows are top-down in GL.
const VIRGL_RESOURCE_Y_0_TOP: u32 = 1;

#[repr(C)]
#[derive(Default)]
struct ResourceInfo {
    handle: u32,
    virgl_format: u32,
    width: u32,
    height: u32,
    depth: u32,
    flags: u32,
    tex_id: u32,
    stride: u32,
    drm_fourcc: c_int,
    fd: c_int,
}

unsafe extern "C" {
    fn virgl_renderer_resource_get_info(res_handle: c_int, info: *mut ResourceInfo) -> c_int;
    fn eglGetProcAddress(name: *const std::ffi::c_char) -> *mut c_void;
    fn eglGetCurrentDisplay() -> *mut c_void;
    fn eglGetCurrentContext() -> *mut c_void;
    fn eglBindAPI(api: u32) -> u32;
    fn eglCreateContext(display: *mut c_void, config: *mut c_void, share: *mut c_void, attribs: *const i32) -> *mut c_void;
    fn eglMakeCurrent(display: *mut c_void, draw: *mut c_void, read: *mut c_void, context: *mut c_void) -> u32;
}

/// The GLES entry points it uses, from ANGLE's eglGetProcAddress.
struct Gl {
    gen_framebuffers: unsafe extern "C" fn(i32, *mut u32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    framebuffer_texture_2d: unsafe extern "C" fn(u32, u32, u32, u32, i32),
    check_framebuffer_status: unsafe extern "C" fn(u32) -> u32,
    gen_buffers: unsafe extern "C" fn(i32, *mut u32),
    bind_buffer: unsafe extern "C" fn(u32, u32),
    buffer_data: unsafe extern "C" fn(u32, isize, *const c_void, u32),
    pixel_storei: unsafe extern "C" fn(u32, i32),
    read_pixels: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
    map_buffer_range: unsafe extern "C" fn(u32, isize, isize, u32) -> *mut c_void,
    unmap_buffer: unsafe extern "C" fn(u32) -> u8,
    get_error: unsafe extern "C" fn() -> u32,
    is_texture: unsafe extern "C" fn(u32) -> u8,
}

impl Gl {
    fn load() -> Option<Gl> {
        unsafe fn get<T: Copy>(name: &CStr) -> Option<T> {
            let p = unsafe { eglGetProcAddress(name.as_ptr()) };
            // SAFETY: T is the entry point's fn pointer type.
            (!p.is_null()).then(|| unsafe { std::mem::transmute_copy(&p) })
        }
        unsafe {
            Some(Gl {
                gen_framebuffers: get(c"glGenFramebuffers")?,
                bind_framebuffer: get(c"glBindFramebuffer")?,
                framebuffer_texture_2d: get(c"glFramebufferTexture2D")?,
                check_framebuffer_status: get(c"glCheckFramebufferStatus")?,
                gen_buffers: get(c"glGenBuffers")?,
                bind_buffer: get(c"glBindBuffer")?,
                buffer_data: get(c"glBufferData")?,
                pixel_storei: get(c"glPixelStorei")?,
                read_pixels: get(c"glReadPixels")?,
                map_buffer_range: get(c"glMapBufferRange")?,
                unmap_buffer: get(c"glUnmapBuffer")?,
                get_error: get(c"glGetError")?,
                is_texture: get(c"glIsTexture")?,
            })
        }
    }
}

/// The worker's readback context and its objects.
struct Reader {
    gl: Gl,
    display: *mut c_void,
    context: *mut c_void,
    fbo: u32,
    pbo: u32,
    pbo_size: usize,
}

enum Slot {
    Untried,
    Ready(Reader),
    /// It couldn't start: always fall back.
    Off,
}

/// Readbacks done this way, for the tests.
pub static READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    static READER: RefCell<Slot> = const { RefCell::new(Slot::Untried) };
}

impl Reader {
    /// Made with vrend's context current, to share its textures.
    fn start() -> Option<Reader> {
        let gl = Gl::load()?;
        unsafe {
            let (display, share) = (eglGetCurrentDisplay(), eglGetCurrentContext());
            if display.is_null() || share.is_null() {
                return None;
            }
            eglBindAPI(EGL_OPENGL_ES_API);
            let attribs = [EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE];
            let context = eglCreateContext(display, std::ptr::null_mut(), share, attribs.as_ptr());
            if context.is_null() || eglMakeCurrent(display, std::ptr::null_mut(), std::ptr::null_mut(), context) == 0 {
                return None;
            }
            let (mut fbo, mut pbo) = (0, 0);
            (gl.gen_framebuffers)(1, &mut fbo);
            (gl.gen_buffers)(1, &mut pbo);
            (gl.pixel_storei)(GL_PACK_ALIGNMENT, 4);
            let ok = (gl.get_error)() == GL_NO_ERROR;
            eglMakeCurrent(display, std::ptr::null_mut(), std::ptr::null_mut(), share);
            ok.then_some(Reader { gl, display, context, fbo, pbo, pbo_size: 0 })
        }
    }

    /// `rect` of the texture into `dst` (rows `stride` apart), in the
    /// guest's format. With this reader's context current.
    unsafe fn read(&mut self, tex: u32, format: Format, rect: Rect, dst: &mut [u8], stride: usize) -> bool {
        let gl = &self.gl;
        let (w, h) = (rect.width as usize, rect.height as usize);
        let row = w * 4;
        let size = row * h;
        unsafe {
            // A buffer resource's id names a GL buffer, not a texture.
            if (gl.is_texture)(tex) == 0 {
                return false;
            }
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, self.fbo);
            (gl.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            if (gl.check_framebuffer_status)(GL_READ_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
                let _ = (gl.get_error)();
                return false;
            }
            (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, self.pbo);
            if self.pbo_size < size {
                (gl.buffer_data)(GL_PIXEL_PACK_BUFFER, size as isize, std::ptr::null(), GL_STREAM_READ);
                self.pbo_size = size;
            }
            (gl.read_pixels)(rect.x as i32, rect.y as i32, w as i32, h as i32, GL_RGBA, GL_UNSIGNED_BYTE, std::ptr::null_mut());
            let p = (gl.map_buffer_range)(GL_PIXEL_PACK_BUFFER, 0, size as isize, GL_MAP_READ_BIT) as *const u8;
            let ok = !p.is_null();
            if ok {
                let src = std::slice::from_raw_parts(p, size);
                let bgr = matches!(format, Format::B8G8R8A8 | Format::B8G8R8X8);
                for (y, s) in src.chunks_exact(row).enumerate() {
                    let d = &mut dst[y * stride..y * stride + row];
                    if bgr {
                        for (d, s) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
                        }
                    } else {
                        d.copy_from_slice(s);
                    }
                }
                (gl.unmap_buffer)(GL_PIXEL_PACK_BUFFER);
            }
            (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, 0);
            (gl.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, 0);
            ok && (gl.get_error)() == GL_NO_ERROR
        }
    }
}

/// Read `rect` of resource `res` as `transfer_read_iov` would, on the
/// worker; false when it can't (the caller falls back).
pub fn read_back(res: u32, rect: Rect, dst: &mut [u8], stride: usize) -> bool {
    let mut info = ResourceInfo::default();
    if unsafe { virgl_renderer_resource_get_info(res as c_int, &mut info) } != 0 || info.tex_id == 0 {
        return false;
    }
    let Some(format) = Format::from_u32(info.virgl_format).filter(|f| {
        matches!(f, Format::B8G8R8A8 | Format::B8G8R8X8 | Format::R8G8B8A8 | Format::R8G8B8X8)
    }) else {
        return false;
    };
    let fits = rect.x + rect.width <= info.width && rect.y + rect.height <= info.height && info.depth <= 1;
    let rows = (rect.height as usize).saturating_sub(1) * stride + rect.width as usize * 4;
    if info.flags & VIRGL_RESOURCE_Y_0_TOP != 0 || !fits || rect.width == 0 || rect.height == 0 || dst.len() < rows {
        return false;
    }
    READER.with_borrow_mut(|slot| {
        if let Slot::Untried = slot {
            *slot = match Reader::start() {
                Some(r) => Slot::Ready(r),
                None => {
                    eprintln!("gpu: no PBO readback, reading scanouts back the slow way");
                    Slot::Off
                }
            };
        }
        let Slot::Ready(reader) = slot else { return false };
        unsafe {
            let previous = eglGetCurrentContext();
            if eglMakeCurrent(reader.display, std::ptr::null_mut(), std::ptr::null_mut(), reader.context) == 0 {
                return false;
            }
            let ok = reader.read(info.tex_id, format, rect, dst, stride);
            eglMakeCurrent(reader.display, std::ptr::null_mut(), std::ptr::null_mut(), previous);
            if ok {
                READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            ok
        }
    })
}
