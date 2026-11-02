//! The berth-virgl worker: the one thread every EGL and virglrenderer call
//! runs on. ANGLE's contexts are current per OS thread and virglrenderer
//! only calls make_current when it switches contexts, so a call from any
//! other thread would find no current context (or EGL_BAD_ACCESS). The
//! device's dispatch queue has no fixed thread, so it never calls the
//! renderer itself: it hands the worker jobs and waits for each.
//!
//! Fences: virglrenderer reports them from `virgl_renderer_poll`, through
//! `write_fence` / `write_context_fence` (virgl.rs), which land in
//! `SIGNALLED`. The worker polls after every batch of jobs and, while any
//! fence is outstanding, at least every millisecond (or when
//! `virgl_renderer_get_poll_fd` says so, where it has one: THREAD_SYNC
//! needs eventfd, so not on macOS), then hands what retired to the sink.
//!
//! One worker per process, like virglrenderer itself: started by the first
//! `VirglWorker::get`, it then lives as long as the process.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;

use super::backend::SignalSink;
use super::fence::Signal;
use super::virgl::ffi;

type Job = Box<dyn FnOnce(&mut State) + Send + 'static>;

/// The worker's own state, which jobs may change.
#[derive(Default)]
pub struct State {
    /// The newest global fence created and not yet signalled.
    global: Option<u32>,
    /// Per (context, ring): the newest fence created and not yet signalled.
    rings: HashMap<(u32, u32), u64>,
    /// Where signals go.
    pub sink: Option<SignalSink>,
}

impl State {
    pub fn fence_created(&mut self, s: Signal) {
        match s {
            Signal::Global(seq) => self.global = Some(seq),
            Signal::Ring { ctx, ring, fence } => {
                self.rings.insert((ctx, ring), fence);
            }
        }
    }

    /// A destroyed context's fences never signal: stop waiting for them.
    pub fn forget_context(&mut self, ctx: u32) {
        self.rings.retain(|&(c, _), _| c != ctx);
    }

    fn outstanding(&self) -> bool {
        self.global.is_some() || !self.rings.is_empty()
    }

    fn retired(&mut self, signals: &[Signal]) {
        for s in signals {
            match *s {
                Signal::Global(seq) => {
                    if self.global.is_some_and(|newest| (seq.wrapping_sub(newest) as i32) >= 0) {
                        self.global = None;
                    }
                }
                Signal::Ring { ctx, ring, fence } => {
                    if self.rings.get(&(ctx, ring)).is_some_and(|&newest| fence >= newest) {
                        self.rings.remove(&(ctx, ring));
                    }
                }
            }
        }
    }
}

/// Fences virglrenderer reported, from its callbacks (worker thread).
static SIGNALLED: Mutex<Vec<Signal>> = Mutex::new(Vec::new());

pub(super) fn signalled(s: Signal) {
    SIGNALLED.lock().unwrap_or_else(|e| e.into_inner()).push(s);
}

pub struct VirglWorker {
    jobs: mpsc::Sender<Job>,
    /// A byte written here wakes the worker for a job.
    wake: OwnedFd,
    thread: ThreadId,
}

static WORKER: OnceLock<Result<VirglWorker, String>> = OnceLock::new();

impl VirglWorker {
    /// The worker, started (with the renderer, on its thread) by the first
    /// call. An error if ANGLE or virglrenderer didn't start, every time.
    pub fn get() -> Result<&'static VirglWorker, String> {
        WORKER.get_or_init(start).as_ref().map_err(Clone::clone)
    }

    /// Run `f` on the worker and wait for it. Not from the worker itself
    /// (a job can't wait for another).
    pub fn run<R: Send>(&self, f: impl FnOnce(&mut State) -> R + Send) -> Result<R, String> {
        if std::thread::current().id() == self.thread {
            return Err("a berth-virgl job can't run another".into());
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let job: Box<dyn FnOnce(&mut State) + Send + '_> = Box::new(move |s| {
            let _ = tx.send(f(s));
        });
        // SAFETY: we wait below until the job has run, or been dropped
        // with its sender, so whatever it borrows outlives it.
        let job: Job = unsafe { std::mem::transmute(job) };
        self.jobs.send(job).map_err(|_| "the berth-virgl worker stopped".to_string())?;
        let _ = unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        rx.recv().map_err(|_| "the berth-virgl worker stopped".into())
    }
}

fn start() -> Result<VirglWorker, String> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("berth-virgl: pipe: {}", std::io::Error::last_os_error()));
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read, &write] {
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    let (jobs, queue) = mpsc::channel::<Job>();
    let (ready_tx, ready) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("berth-virgl".into())
        .spawn(move || {
            let started = ffi::Renderer::start();
            let ok = started.is_ok();
            let _ = ready_tx.send(started);
            if ok {
                serve(queue, read);
            }
        })
        .map_err(|e| format!("berth-virgl: {e}"))?;
    let id = thread.thread().id();
    ready.recv().map_err(|_| "berth-virgl died starting".to_string())??;
    Ok(VirglWorker { jobs, wake: write, thread: id })
}

/// The worker's loop: jobs as they come, and fences while any are due.
fn serve(queue: mpsc::Receiver<Job>, wake: OwnedFd) {
    let mut state = State::default();
    loop {
        let fence_fd = unsafe { ffi::virgl_renderer_get_poll_fd() };
        let mut fds = [
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: fence_fd, events: libc::POLLIN, revents: 0 },
        ];
        let n = if fence_fd >= 0 && state.outstanding() { 2 } else { 1 };
        let timeout = if state.outstanding() { 1 } else { -1 };
        unsafe { libc::poll(fds.as_mut_ptr(), n, timeout) };
        let mut buf = [0u8; 64];
        while unsafe { libc::read(wake.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        loop {
            match queue.try_recv() {
                Ok(job) => job(&mut state),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        unsafe { ffi::virgl_renderer_poll() };
        let retired = std::mem::take(&mut *SIGNALLED.lock().unwrap_or_else(|e| e.into_inner()));
        if !retired.is_empty() {
            state.retired(&retired);
            if let Some(sink) = &state.sink {
                sink(retired);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outstanding_fences() {
        let mut s = State::default();
        assert!(!s.outstanding());
        s.fence_created(Signal::Global(5));
        s.fence_created(Signal::Global(6));
        s.retired(&[Signal::Global(5)]);
        assert!(s.outstanding(), "6 is still due");
        s.retired(&[Signal::Global(6)]);
        assert!(!s.outstanding());
        s.fence_created(Signal::Ring { ctx: 1, ring: 0, fence: 1 << 40 });
        s.retired(&[Signal::Ring { ctx: 1, ring: 1, fence: 1 << 41 }]);
        assert!(s.outstanding());
        s.forget_context(1);
        assert!(!s.outstanding());
    }
}
