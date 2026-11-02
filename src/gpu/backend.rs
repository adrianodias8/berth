//! The 3D renderer as server.rs sees it. `VirglBackend` (virgl.rs, behind
//! the `virgl` feature) is the real one, a proxy to the berth-virgl worker
//! thread; `FakeBackend` (tests) records what it's asked and signals fences
//! when a test says so, so the routing of every 3D command is tested on
//! any Mac without ANGLE.
//!
//! Errors are the renderer's own codes (errno-like); server.rs answers the
//! guest ERR_UNSPEC for them, as QEMU does. Pointers handed over (backing
//! iovecs, read-back buffers) are guest RAM mappings or host buffers that
//! the caller keeps alive for as long as the call says.

use super::fence::Signal;
use super::protocol::{Create3d, Rect, Transfer3d};
use super::server::Capset;

/// One piece of guest memory: start and length.
pub type Iov = (*mut u8, usize);

/// Where fence signals go: called on the renderer's thread, so it must
/// only hand them on (device.rs posts them onto the device queue).
pub type SignalSink = Box<dyn Fn(Vec<Signal>) + Send>;

/// RESOURCE_CREATE_BLOB as the renderer takes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct BlobArgs {
    pub resource_id: u32,
    pub ctx_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub blob_id: u64,
    pub size: u64,
}

pub trait Backend3d: Send {
    /// The capability sets the guest may ask for (VIRGL, VIRGL2, ...).
    fn capsets(&mut self) -> Vec<Capset>;

    fn ctx_create(&mut self, ctx: u32, capset_id: u32, name: &[u8]) -> Result<(), i32>;
    fn ctx_destroy(&mut self, ctx: u32);
    fn ctx_attach_resource(&mut self, ctx: u32, res: u32);
    fn ctx_detach_resource(&mut self, ctx: u32, res: u32);

    fn resource_create_3d(&mut self, args: &Create3d) -> Result<(), i32>;
    fn resource_unref(&mut self, res: u32);
    /// The resource's backing, which stays mapped until `detach_iov` or
    /// `resource_unref`: the renderer reads and writes it directly.
    fn attach_iov(&mut self, res: u32, iovs: &[Iov]) -> Result<(), i32>;
    fn detach_iov(&mut self, res: u32);

    fn transfer_to_host_3d(&mut self, ctx: u32, t: &Transfer3d) -> Result<(), i32>;
    fn transfer_from_host_3d(&mut self, ctx: u32, t: &Transfer3d) -> Result<(), i32>;
    /// A command stream, a whole number of 32-bit words.
    fn submit_3d(&mut self, ctx: u32, commands: &[u8]) -> Result<(), i32>;

    /// A global fence, signalled as `Signal::Global(host_seq)` once the
    /// work submitted before it is done.
    fn create_fence(&mut self, host_seq: u32) -> Result<(), i32>;
    /// A fence on one of a context's rings, signalled as `Signal::Ring`.
    fn create_context_fence(&mut self, ctx: u32, ring: u32, fence: u64) -> Result<(), i32>;
    fn set_signal_sink(&mut self, sink: SignalSink);

    /// Copy `rect` (in the resource's own rows, as the renderer stores
    /// them) into `dst`: pixel (rect.x, rect.y) at dst[0], rows `stride`
    /// bytes apart, in the resource's format.
    fn read_back(&mut self, res: u32, rect: Rect, dst: &mut [u8], stride: usize) -> Result<(), i32>;

    fn resource_create_blob(&mut self, args: &BlobArgs, iovs: &[Iov]) -> Result<(), i32>;
    /// Host memory behind a mappable blob: start and length.
    fn resource_map(&mut self, res: u32) -> Result<(*mut u8, u64), i32>;
    fn resource_unmap(&mut self, res: u32) -> Result<(), i32>;
    /// The guest's caching for a mapped blob (protocol::map_cache).
    fn map_info(&mut self, res: u32) -> Result<u32, i32>;
}

#[cfg(test)]
pub mod fake {
    //! A renderer that records its calls.

    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::gpu::protocol::capset;

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub enum Call {
        CtxCreate(u32, u32, Vec<u8>),
        CtxDestroy(u32),
        CtxAttach(u32, u32),
        CtxDetach(u32, u32),
        Create3d(Create3d),
        Unref(u32),
        /// The resource and each iovec's (address, length).
        AttachIov(u32, Vec<(usize, usize)>),
        DetachIov(u32),
        ToHost(u32, Transfer3d),
        FromHost(u32, Transfer3d),
        Submit(u32, Vec<u8>),
        Fence(u32),
        ContextFence(u32, u32, u64),
        ReadBack(u32, Rect, usize),
        CreateBlob(BlobArgs, usize),
        Map(u32),
        Unmap(u32),
    }

    /// What the test sees of the backend after the server took it.
    #[derive(Default)]
    pub struct Shared {
        pub calls: Vec<Call>,
        pub sink: Option<SignalSink>,
        /// Contexts that exist, so submits to others fail as virgl's do.
        pub contexts: Vec<u32>,
        /// What read_back writes: its pixel at (x, y) of the resource.
        pub pixel: Option<fn(u32, u32) -> [u8; 4]>,
        /// The memory resource_map hands out.
        pub map: Option<(usize, u64)>,
    }

    pub struct FakeBackend {
        pub shared: Arc<Mutex<Shared>>,
        pub capsets: Vec<Capset>,
    }

    impl FakeBackend {
        /// One with the VIRGL2 capset, and a handle on what it records.
        pub fn new() -> (Self, Arc<Mutex<Shared>>) {
            let shared = Arc::new(Mutex::new(Shared::default()));
            let capsets = vec![Capset { id: capset::VIRGL2, max_version: 2, data: vec![2; 8] }];
            (Self { shared: shared.clone(), capsets }, shared)
        }
    }

    /// Signal fences, as the renderer would from its thread.
    pub fn fire(shared: &Arc<Mutex<Shared>>, signals: Vec<Signal>) {
        let s = shared.lock().unwrap();
        (s.sink.as_ref().expect("no signal sink"))(signals);
    }

    pub fn calls(shared: &Arc<Mutex<Shared>>) -> Vec<Call> {
        std::mem::take(&mut shared.lock().unwrap().calls)
    }

    impl FakeBackend {
        fn record(&self, c: Call) {
            self.shared.lock().unwrap().calls.push(c);
        }
    }

    impl Backend3d for FakeBackend {
        fn capsets(&mut self) -> Vec<Capset> {
            self.capsets.clone()
        }
        fn ctx_create(&mut self, ctx: u32, capset_id: u32, name: &[u8]) -> Result<(), i32> {
            self.record(Call::CtxCreate(ctx, capset_id, name.to_vec()));
            self.shared.lock().unwrap().contexts.push(ctx);
            Ok(())
        }
        fn ctx_destroy(&mut self, ctx: u32) {
            self.record(Call::CtxDestroy(ctx));
            self.shared.lock().unwrap().contexts.retain(|&c| c != ctx);
        }
        fn ctx_attach_resource(&mut self, ctx: u32, res: u32) {
            self.record(Call::CtxAttach(ctx, res));
        }
        fn ctx_detach_resource(&mut self, ctx: u32, res: u32) {
            self.record(Call::CtxDetach(ctx, res));
        }
        fn resource_create_3d(&mut self, args: &Create3d) -> Result<(), i32> {
            self.record(Call::Create3d(*args));
            Ok(())
        }
        fn resource_unref(&mut self, res: u32) {
            self.record(Call::Unref(res));
        }
        fn attach_iov(&mut self, res: u32, iovs: &[Iov]) -> Result<(), i32> {
            self.record(Call::AttachIov(res, iovs.iter().map(|&(p, n)| (p as usize, n)).collect()));
            Ok(())
        }
        fn detach_iov(&mut self, res: u32) {
            self.record(Call::DetachIov(res));
        }
        fn transfer_to_host_3d(&mut self, ctx: u32, t: &Transfer3d) -> Result<(), i32> {
            self.record(Call::ToHost(ctx, *t));
            Ok(())
        }
        fn transfer_from_host_3d(&mut self, ctx: u32, t: &Transfer3d) -> Result<(), i32> {
            self.record(Call::FromHost(ctx, *t));
            Ok(())
        }
        fn submit_3d(&mut self, ctx: u32, commands: &[u8]) -> Result<(), i32> {
            self.record(Call::Submit(ctx, commands.to_vec()));
            if self.shared.lock().unwrap().contexts.contains(&ctx) { Ok(()) } else { Err(libc::EINVAL) }
        }
        fn create_fence(&mut self, host_seq: u32) -> Result<(), i32> {
            self.record(Call::Fence(host_seq));
            Ok(())
        }
        fn create_context_fence(&mut self, ctx: u32, ring: u32, fence: u64) -> Result<(), i32> {
            self.record(Call::ContextFence(ctx, ring, fence));
            Ok(())
        }
        fn set_signal_sink(&mut self, sink: SignalSink) {
            self.shared.lock().unwrap().sink = Some(sink);
        }
        fn read_back(&mut self, res: u32, rect: Rect, dst: &mut [u8], stride: usize) -> Result<(), i32> {
            self.record(Call::ReadBack(res, rect, stride));
            let pixel = self.shared.lock().unwrap().pixel;
            if let Some(pixel) = pixel {
                for r in 0..rect.height as usize {
                    for c in 0..rect.width as usize {
                        let at = r * stride + c * 4;
                        dst[at..at + 4].copy_from_slice(&pixel(rect.x + c as u32, rect.y + r as u32));
                    }
                }
            }
            Ok(())
        }
        fn resource_create_blob(&mut self, args: &BlobArgs, iovs: &[Iov]) -> Result<(), i32> {
            self.record(Call::CreateBlob(*args, iovs.len()));
            Ok(())
        }
        fn resource_map(&mut self, res: u32) -> Result<(*mut u8, u64), i32> {
            self.record(Call::Map(res));
            let map = self.shared.lock().unwrap().map;
            map.map(|(p, n)| (p as *mut u8, n)).ok_or(libc::EINVAL)
        }
        fn resource_unmap(&mut self, res: u32) -> Result<(), i32> {
            self.record(Call::Unmap(res));
            Ok(())
        }
        fn map_info(&mut self, _res: u32) -> Result<u32, i32> {
            Ok(crate::gpu::protocol::map_cache::CACHED)
        }
    }
}
