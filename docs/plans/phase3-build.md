# Phase 3 build plan: 3D, live display, Omarchy

Merged from two independent designs (phase3-design workflow, 2026-11-02).

## Summary

Base: DESIGN B, with grafts from DESIGN A.

**Why B.** B follows the plan's section 2 design: sessions rather than one global screen, the host keeping the latest frame of every surface, a Metal layer paced to the display link, and the KMS scanout on card0 kept only as a fallback. It is also the only design that covers Phase 4's "`berth machine run dev -- firefox` is one Mac window" and the Agents peek. In A, every headless sway/cage session stays on grim, and only the single card0 desktop is live.

B's slices are also more cleanly independent:
- each file has one owner;
- they share two contracts written down up front (the agent record format and the ring layout);
- `Backend3d` is a trait with a FakeBackend, so the 3D routing is tested on the host without ANGLE.

**Where B is weak, and what A fixes.**
1. **Thread confinement.** B runs virgl directly on the "dev.berth.gpu" GCD serial queue. A serial queue does not keep a fixed OS thread, ANGLE contexts are current per thread, and virglrenderer only calls make_current when it switches contexts. So a thread hop leaves GL with no current context. **Graft A's dedicated `berth-virgl` worker thread.** Server, the RefCell and the virtqueue elements stay on the serial queue, and the worker posts fence completions back to it.
2. **Phase 3 at display rate.** B's route depends on wl_shm screencopy over vsock. For full-frame glmark2 that is about 500 MB/s and untested, plus a guest-side GPU readback through virgl. **Graft A's KMS route for the done-when:** a desktop session with a sway-on-DRM flavour on card0. Its frames come out as a 3D scanout read back on the host into `SURFACES[Scanout(0)]`, with no vsock. That makes Phase 3 provable even if vsock throughput disappoints. Hyprland/Omarchy uses the same card0 route.

**Also grafted from A:**
- A's `virgl_clear_reads_back_red` test: a real draw, alongside B's copy round trip;
- A's damage-history and pacing tests;
- A's `berth gpu-probe` behaviour (exit codes);
- `Machine::spawn_root` for seatd. This works on every distro, alongside B's seat-group membership;
- A's rule that a failed renderer start falls back to the 2D device;
- removing the `BERTH_GPU_VIRGL_CAPSETS` gate, which today offers F_VIRGL while every 3D command fails.

**Kept from B:**
- the ring transport;
- per-session feeders;
- `once` screenshots;
- the res_id fields, reserved now and built after S3 passes;
- `save_state` failing only while a 3D context exists. 2D-only snapshots keep working, which A's setSupportsSaveRestore(false) would have lost;
- blob and CONTEXT_INIT behind the `gpu_blob=false` setting.

**Corrected against the code:**
- virglrenderer's global fence IDs are 32-bit (`write_fence(cookie, u32)`; `create_fence` takes int), while the guest's fence_id is u64. So slice A maps them through its own counter.
- `save_state` returns `Option<NSData>`. Returning None to fail a save needs a live check that snapshots.rs then falls back to the disk.

The result is 6 slices (A-F) that can be built in parallel, then G, a small wiring and acceptance slice that proves Phase 3 and the Omarchy part of Phase 4.

## Architecture

LAYERS (one owner each; paths relative to the repo root)

**1. Device: virtio-gpu 3D** (src/gpu, slice A)

- **Server** (server.rs) stays on the serial queue "dev.berth.gpu", inside the existing `RefCell<Server>`. It gains:
  - `resources: HashMap<u32, Resource{kind: TwoD{host bytes} | ThreeD{args, staging: Option<Vec<u8>>} | Blob{..}}>`;
  - `contexts: HashMap<u32, Context{capset_id, name, resources: HashSet<u32>}>`;
  - `backend: Option<Box<dyn Backend3d>>`.

- **Backend3d** (new src/gpu/backend.rs) is a `Send` trait. FakeBackend implements it for tests.
  - `VirglBackend` (virgl.rs, cfg feature "virgl") is a proxy to the **berth-virgl worker thread** (new src/gpu/virgl_thread.rs, grafted from A).
  - The worker calls `Renderer::get()`, so every EGL/ANGLE context lives on that one thread.
  - Each trait call is sent to the worker as a boxed job over an mpsc channel, and the caller waits for the reply.
  - Every job's errors come back synchronously.

- **execute.** `Server::handle` passes `&req.hdr` into `execute`. Today only `req.cmd` is passed (server.rs:199-236). `execute` returns:
  - `Reply::Now(bytes)`;
  - `Reply::Fenced{ctx, ring, fence, bytes}` for 3D commands with FLAG_FENCE;
  - `Reply::Deferred{MapRequest}` for RESOURCE_MAP_BLOB (gpu_blob only).

- **Fences** (new src/gpu/fence.rs).
  - `GpuDevice::drain` (device.rs:309) still pulls `nextElement` until the queue is empty, because notifications stay off until it is drained. Fenced elements are kept as `Retained<VZVirtioQueueElement>` in `Ivars.pending` (per-(ctx, ring) FIFOs plus a global FIFO) instead of being returned.
  - The worker calls `virgl_renderer_poll()` after each job, and while fences are outstanding it also polls `virgl_renderer_get_poll_fd()` when that is ≥0, with a 1 ms timeout otherwise.
  - The `write_fence` (u32) and `write_context_fence` (u64) callbacks fire on the worker thread. They record signals, and the worker `dispatch_async`s a completion block onto "dev.berth.gpu".
  - The completion block calls `pending.complete(signals)`, then writes and returns each finished element in order.
  - virglrenderer's global fence IDs are 32-bit, so Server hands it a host u32 sequence and keeps a map back to the guest's u64 fence_id.

- **Features** (device.rs:398-401):
  - F_VIRGL when a backend started and has capsets;
  - F_CONTEXT_INIT | F_RESOURCE_BLOB only when `gpu_blob=true` (default false, to avoid the dumb-buffer-as-blob trap at device.rs:11-14);
  - EDID and RESOURCE_UUID as today.

- **Settings.**
  - Config `gpu_virgl` (default true when built with the feature) and the kill switch `BERTH_GPU_VIRGL=0` replace `BERTH_GPU_VIRGL_CAPSETS`.
  - If the renderer fails to start, the device falls back to today's 2D device (logged).

- **3D scanout.** SET_SCANOUT / RESOURCE_FLUSH on a ThreeD resource:
  - the worker's `read_back(res, rect)` does transfer_read_iov into the resource's staging Vec;
  - then the existing `convert`/`publish` writes to `frames::DISPLAY.update(scanout, …)`;
  - formats map as B8G8R8A8=1, B8G8R8X8=2, R8G8B8A8=67, R8G8B8X8=134;
  - rows are flipped when the resource is not y_0_top.
  So any KMS client on card0 reaches the host with no new host path.

- **read_resource.** `pub fn read_resource(res_id, rect, dst, stride)` dispatch_syncs onto the gpu queue, for C's future res_id records.

- **Saved state.** `save_state` returns None while any 3D context exists, so the VM save fails and berth falls back to disk; the live check confirms that. Otherwise save_state is unchanged. `reset` destroys the contexts.

**2. Packaging** (slice B)

- When CARGO_FEATURE_VIRGL is set, build.rs emits:
  - a link search path to the prefix (BERTH_VIRGL_PREFIX, else target/spikes/gpu/prefix);
  - rpaths `@executable_path/../Frameworks` first, then the prefix lib.
- build.sh: `BERTH_VIRGL=auto|1|0`.
- build-app.sh:
  - copies the @rpath dylib closure (virglrenderer, epoxy, EGL, GLESv2) into Contents/Frameworks;
  - makes sure libepoxy resolves its dlopen;
  - deletes the absolute rpath from the bundled CLI;
  - signs each dylib ad-hoc, then the inner berth, then the bundle.
- check-bundle.sh enforces all of this.
- Hidden `berth gpu-probe`.
- No hardened runtime.

**3. Surfaces** (slice C: src/gpu/frames.rs, src/gpu/ring.rs, src/gui/display.rs, src/daemon.rs)

- `frames::SURFACES` is a registry keyed by `SurfaceKey::{Session{machine, session}, Scanout(u32)}`.
- `frames::DISPLAY` stays as a facade for `Scanout(n)`, keeping `update`/`latest`/`set_cursor`/`move_cursor`, so server.rs and Dump::tick compile unchanged.

- **Ring file.** Each surface owns `~/.berth/run/surfaces/<key>.<gen>.ring`, mapped with mmap.
  - A 16 KiB header holds: magic "BRTHSURF", v1, gen, w, h, stride = align_up(w*4, 256), format bgra8, slots=3, slot_offset[3], slot_seq[3] (seqlock, odd while writing), latest, and cursor{x, y, hot_x, hot_y, w, h, visible, seq}.
  - Then 3 slots, each 16 KiB-aligned, then a 256x256 BGRA cursor image.
  - A resize creates a new gen file.
  - Damage history is the last 8 to 16 entries per seq; when it doesn't reach back far enough, a full frame is sent.
  - Wakeups use a Condvar: `wait_newer(key, seq, timeout)`.

- **Display server.** The new control request `{"cmd":"display", machine, session?|scanout?, max_fps (default 60, 1..=120), once}` returns early like Gui.
  - The reply line is `{ok, transports:["ring"], ring, gen, width, height, stride, format:"bgra8", slots:3, live}`.
  - Then come doorbell lines: `{"ev":"frame","slot","seq","damage":[[x,y,w,h]]}`, `{"ev":"resize",…}`, `{"ev":"cursor","seq"}`, `{"ev":"end","error"}`.
  - On the same socket the app sends input lines: `{"ev":"pointer","x","y"}`, `{"ev":"button","button":272|273|274,"down"}`, `{"ev":"scroll","dx","dy","discrete"}`, `{"ev":"key","code":<evdev>,"down"}`, `{"ev":"release_all"}`, `{"ev":"control","on"}`.

- **Feeder.**
  - The first subscriber to a Session key starts a feeder. It opens `guest_stream("gui <machine>")` (daemon.rs:716), sends the agent verb `screen.stream`, and writes the records into the ring.
  - It stops 10 s after the last subscriber.
  - Scanout keys need no feeder: A fills them.

- **Input.** While control is on, input is forwarded to one daemon-held `input.stream` agent connection per session. Each control on/off is logged with who=berth-app.

- **once.** `once:true` returns a PNG from the registry, giving a fast `berth gui screenshot` while a feeder is live.

**4. Agent streams** (slice D)

- `screen.stream` and `input.stream` are the documented exception to one request per connection.
- The agent pumps stdio between vsock and the session's `$RT/berth-gui-input gui-capture` / `gui-input` processes, which run as the user through `Machine::spawn_piped`.

- **gui-capture** is a raw-wire zwlr_screencopy_v1 (v3) client.
  - It uses wl_shm through a memfd sent with SCM_RIGHTS, which is new fd support in wayland.rs Conn, and copy_with_damage.
  - Each record is a JSON line `{"seq","w","h","stride","format":"bgra8","damage":[[x,y,w,h]],"payload_len"}`, followed by the packed rows of each damaged rect. The first frame is always a full frame.
  - Reserved: `{"format":"resource","res_id","damage","payload_len":0}`.

- **gui-input** keeps one zwlr_virtual_pointer_v1 and one zwp_virtual_keyboard_v1, with an embedded US xkb keymap sent as a memfd. It tracks the modifier mask and releases everything on EOF.

**5. Guest sessions** (slice E)

- gles2 sessions export WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128 and retry once with pixman.
- Mesa is installed explicitly.
- The render node and card0 binds are retrofitted onto existing machines.

- **Kind "desktop"** has two flavours:
  - `sway`: WLR_BACKENDS=drm, WLR_DRM_DEVICES=/dev/dri/card0 (grafted from A; this is the Phase 3 vehicle);
  - `hyprland`: AQ_DRM_DEVICES=/dev/dri/card0, `uwsm start -- hyprland.desktop` when Omarchy is installed, else `Hyprland` (Phase 4).

- **Seat.** seatd runs through `Machine::spawn_root` with SEATD_VTBOUND=0, and the user joins the seat group.

- **Rules.**
  - At most one desktop per VM, because DRM master on card0 is exclusive.
  - The session.new and session.list replies carry kind, renderer and, for desktop, "scanout":0.
  - hypr.rs maps `hyprctl clients -j` into sway::Window.

**6. App** (slice F)

- `DisplayBackend.stream`: SurfaceStream sends the display request, either for a session or for scanout 0 when the session JSON has "scanout". It maps the ring with mmap and makes one MTLBuffer(bytesNoCopy:) per gen.
- MetalFrameView is a CAMetalLayer (bgra8Unorm) driven by `NSView.displayLink`.
  - On each tick it blits the newest slot through a linear texture into a private texture.
  - When the command buffer completes, it rechecks the seqlock and drops torn frames.
  - It draws the cursor overlay, then presents.
- "Take control" input: mac keycode→evdev, aspect-fit plus y-flip.
- SessionPeek reads the ring at 2 fps.
- The grim screenshot backend remains the fallback.

**KEY DECISIONS AND WHY**

- **A worker thread owns GL; the serial queue owns the virtqueues** (from A). GCD does not pin a queue to a thread, and ANGLE contexts are current per thread. Elements and Server never leave "dev.berth.gpu", so RefCell stays sound.
- **Two frame sources into one registry.**
  - *Session capture over vsock (B)* covers every headless session: agent, app and desktop. It gives per-session windows, the peek and Phase 4's firefox window.
  - *Host-side KMS scanout readback (A)* covers the card0 desktop at display rate with no vsock cost. This is the Phase 3 proof.
  - The app treats both as surfaces.
- **The ring is the first transport, not IOSurface or XPC.** The daemon is not launchd-managed, and the ring is zero-copy on the app side. `transports` leaves room for "iosurface-xpc" later.
- **Records carry pixels now and res_id later.** Phase 4's dmabuf path after S3 changes only the agent's buffer allocation and the feeder's source.
- **Blob and CONTEXT_INIT ship off by default,** with code and fake tests, keeping the Venus hooks: capset passthrough, ring_idx fences, the shared-memory map.
- **Contracts frozen before the slices start:** docs/display-protocol.md (ring layout, doorbell, input lines, agent records), owned by C, with D and F coding against the copy in this plan. D and E each touch disjoint functions of agent/src/gui/mod.rs; merge D first.

**FILE OWNERSHIP**
- **A:** src/gpu/{server,device,protocol,virgl,backend*,fence*,virgl_thread*,mod}.rs, src/config.rs.
- **B:** Cargo.toml, build.rs, scripts/{build,build-app,sign,check-bundle*}.sh, src/main.rs (gpu-probe only), docs/configuration.md.
- **C:** src/gpu/{frames,ring*}.rs, src/gui/{mod,display*}.rs, src/daemon.rs, docs/display-protocol.md*.
- **D:** agent/src/gui/{wayland,capture*,input*,exec}.rs, agent/src/gui/keymap-us.xkb*, agent/src/main.rs, and the serve/dispatch/handle_stream region of agent/src/gui/mod.rs, plus docs/gui-protocol.md.
- **E:** agent/src/gui/{install,hypr*}.rs, agent/src/machines.rs, and the session_new/launcher/prelude/kind-dispatch region of agent/src/gui/mod.rs, plus docs/gui.md. E's seatd spawn_root goes into exec.rs after D's spawn_piped (append-only).
- **F:** app/**.
- **G:** one-line wiring plus docs/LEARNINGS.md.

## Slices

### A-virgl-device: virgl 3D in the device: worker thread, contexts and 3D resources, deferred fences, 3D scanout readback; blob/CONTEXT_INIT behind a flag

Depends on: nothing

Files: `src/gpu/backend.rs (new)`, `src/gpu/virgl_thread.rs (new)`, `src/gpu/fence.rs (new)`, `src/gpu/server.rs`, `src/gpu/device.rs`, `src/gpu/protocol.rs`, `src/gpu/virgl.rs`, `src/gpu/mod.rs`, `src/config.rs`

**Work.** **1. backend.rs: `pub trait Backend3d: Send`.**
- Methods:
  - `capsets()`;
  - `ctx_create(ctx, capset_id, name)`, `ctx_destroy`, `ctx_attach_resource`, `ctx_detach_resource`;
  - `resource_create_3d(&Create3d)`, `resource_unref`;
  - `attach_iov(res, &[(*mut u8, usize)])`, `detach_iov`;
  - `transfer_to_host_3d`, `transfer_from_host_3d`, `submit_3d(ctx, &[u8])`;
  - `create_fence(host_seq: u32)`, `create_context_fence(ctx, ring, fence: u64)`;
  - `set_signal_sink(Box<dyn Fn(Vec<Signal>) + Send>)`;
  - `read_back(res, rect, dst, stride)`;
  - blob: `resource_create_blob`, `resource_map -> (ptr, len)`, `resource_unmap`, `map_info`.
- A cfg(test) `FakeBackend` records calls and signals fences when a test calls `fire()`.

**2. virgl_thread.rs: `VirglWorker`.**
- It spawns the thread `berth-virgl`, and `Renderer::get()` runs on that thread.
- Jobs are boxed `FnOnce(&Renderer) -> R` sent over mpsc, with a oneshot reply.
- After each job, and while fences are outstanding, it polls: on `virgl_renderer_get_poll_fd()` (new FFI) when that is ≥0, with a 1 ms recv_timeout tick otherwise. Each poll calls `virgl_renderer_poll()`.
- `write_fence(u32)` and the new `write_context_fence(ctx, ring, u64)` (callbacks v4) push into a static `SIGNALLED` list. The worker drains that list and calls the sink.
- `VirglBackend` implements Backend3d by proxying to the worker.

**3. virgl.rs.**
- Safe wrappers over the existing FFI.
- Replace the `BERTH_GPU_VIRGL_CAPSETS` gate (virgl.rs:48) with `pub fn backend(&Settings) -> Option<Box<dyn Backend3d>>`. It returns None without the feature, with gpu_virgl=false, with `BERTH_GPU_VIRGL=0`, or when worker start or renderer init fails (logged).
- Add `pub fn probe() -> Result<Vec<Capset>, String>`, which starts a worker, for B.

**4. server.rs.**
- Add Resource.kind, `contexts`, and `backend`. `handle` returns `Reply` and passes `&req.hdr` into `execute`.
- Split the stub arm at server.rs:320-332 into one function per command:
  - an unknown ctx answers RESP_ERR_INVALID_CONTEXT_ID; an unknown resource answers ERR_INVALID_RESOURCE_ID;
  - CtxCreate uses capset_id when CONTEXT_INIT was negotiated, else VIRGL2;
  - ResourceCreate3d checks the size against HOST_PIXEL_LIMIT;
  - Submit3d's length is checked against the payload.
- ATTACH_BACKING / DETACH_BACKING on ThreeD resources: `Backing::map` pieces become iovecs for `attach_iov` / `detach_iov`.
- UNREF calls the backend, removes the resource from every ctx, and unbinds any scanout.
- SET_SCANOUT / RESOURCE_FLUSH on ThreeD:
  - `read_back` into the lazily allocated staging Vec;
  - then the existing convert/publish;
  - format map 1/2/67/134, with a y-flip when not y_0_top.
- TRANSFER_TO_HOST_2D on a ThreeD resource answers ERR_INVALID_PARAMETER.
- FLAG_FENCE:
  - allocate the next host u32 seq, mapped to (guest fence_id, ctx, ring);
  - call `create_fence(seq)`, or `create_context_fence` when FLAG_INFO_RING_IDX is set and CONTEXT_INIT was negotiated;
  - return Reply::Fenced.
- Blob commands only when gpu_blob is on, else ERR_UNSPEC as today:
  - ResourceCreateBlob: GUEST takes iov; HOST3D and HOST3D_GUEST take a blob_id;
  - MapBlob returns Reply::Deferred(MapRequest{offset, ptr, len}) with OkMapInfo;
  - SetScanoutBlob goes through the same readback.
- With no backend, every 3D command answers ERR_UNSPEC exactly as today.
- `save_state` returns Err while any ctx exists. `reset` destroys contexts and 3D resources.

**5. fence.rs: `Pending<E>`.**
- Generic over the element type, so it is host-testable.
- A global FIFO keyed by host seq, plus per-(ctx, ring) FIFOs keyed by u64.
- `complete(&[Signal]) -> Vec<(E, Vec<u8>)>` returns ready elements in order.

**6. device.rs.**
- `drain` queues Fenced and Deferred elements in `Ivars.pending`, and writes Now replies as today.
- The signal sink dispatch_asyncs onto the stored "dev.berth.gpu" queue. The block calls `complete`, then writes and returns each element.
- MapBlob: `mapMemory_atOffset_size_completionHandler` (device.rs:142), with offsets 16 KiB-aligned, else ERR_INVALID_PARAMETER. The completion re-enters the queue, then writes and returns.
- `will_reset` answers every pending element with ERR_UNSPEC, then resets.
- `save_state` returns None when Server says Err.
- Features at 398-401 as in the architecture. `pub fn read_resource(..)` dispatch_syncs onto the queue.

**7. config.rs and mod.rs.** Add `gpu_virgl: Option<bool>` and `gpu_blob: Option<bool>` to config.json and Settings. `attach` builds the backend once, before `configuration`.

**8. Tests.**
- Update hostile_requests_get_errors (server.rs:883): with no backend, Submit3d and blob still answer ERR_UNSPEC; with a FakeBackend, Submit3d on an unknown ctx answers INVALID_CONTEXT_ID.

**Done when (host).** `cargo test -p berth gpu::` (no features) passes, including new FakeBackend tests:
- ctx create and destroy; unknown ctx gives INVALID_CONTEXT_ID;
- create_3d plus attach passes the merged iovecs;
- fenced submit becomes Reply::Fenced echoing fence_id, ctx_id and ring_idx;
- the u64→u32 fence map survives fence_id > 2^32;
- `Pending::complete` releases elements in order, globally and per (ctx, ring), and out-of-order signals don't release later fences early;
- unfenced commands answer Now;
- unref of a 3D resource reaches the backend, unbinds the scanout and leaves the ctxs;
- set_scanout plus flush on a 3D resource calls read_back with the intersected rect and publishes into a test Frames;
- formats 67/134 are swizzled to BGRA, and the y-flip is applied;
- blob answers ERR_UNSPEC unless gpu_blob; with gpu_blob, MapBlob gives Deferred with aligned offsets and a misaligned offset gives INVALID_PARAMETER;
- features: F_VIRGL only with a capset-bearing backend; CONTEXT_INIT and BLOB only with gpu_blob;
- save_state errors while a ctx exists.

`grep -rn BERTH_GPU_VIRGL_CAPSETS src` finds nothing.

With the S2 prefix: `RUSTFLAGS="-L $PWD/target/spikes/gpu/prefix/lib -C link-arg=-Wl,-rpath,$PWD/target/spikes/gpu/prefix/lib" cargo test -p berth --features virgl gpu:: -- --include-ignored` passes (no RUSTFLAGS needed once B lands):
- `probe_renderer`;
- `virgl_roundtrip`: VIRGL2 ctx, a 64x64 B8G8R8A8 resource, transfer_to_host_3d of a pattern, a fenced empty submit signalled within 1 s, transfer_from_host_3d identical;
- `virgl_clear_reads_back_red`: a hand-built CREATE_OBJECT SURFACE, SET_FRAMEBUFFER_STATE, CLEAR (1,0,0,1), a fence, then read_back is all BGRA 00 00 FF FF;
- a call made from a second Rust thread through the backend still works, proving the worker confines GL to one thread.

`cargo clippy -p berth --features virgl` is clean. `cargo build --release -p berth` without the feature builds.

**Live check.** With gui=true, a virgl build, and the VM restarted:
- the daemon log shows capsets VIRGL and VIRGL2 and 'driver ready, features virgl, edid, resource-uuid';
- the guest dmesg shows `[drm] features: +virgl`;
- `/dev/dri/renderD128` exists. Record whether it existed before virgl, which settles the S1 vs S3 disagreement;
- `gpu_has_3d()` is true;
- as root in the VM, `eglinfo -B` shows virgl on ANGLE Metal;
- `kmscube` on card0 appears in `~/.berth/gpu/scanout-0.png` with dump-frames on;
- glmark2-es2-drm completes with no `virtio_gpu fence timeout` in dmesg and no ERR_UNSPEC in the daemon log.

Verify that fenced elements held past `drain` are completed correctly from the dispatched block. This is the key VZ unknown; see the risks.

Suspend with a live 3D ctx falls back to a disk snapshot cleanly. With gpu_blob=true, check that the 2D console still works.

### B-packaging: Build with --features virgl automatically; embed, fix rpaths and sign the dylibs in Berth.app; hidden `berth gpu-probe`

Depends on: nothing

Files: `Cargo.toml`, `build.rs`, `scripts/build.sh`, `scripts/build-app.sh`, `scripts/sign.sh`, `scripts/check-bundle.sh (new)`, `src/main.rs (gpu-probe only)`, `docs/configuration.md`, `bench/spikes/gpu/README.md`

**Work.** **1. build.rs.** When CARGO_FEATURE_VIRGL is set:
- prefix = BERTH_VIRGL_PREFIX, else `<manifest>/target/spikes/gpu/prefix`.
- Panic with "run bench/spikes/gpu/s2-virgl-angle.sh or set BERTH_VIRGL_PREFIX" unless libvirglrenderer*.dylib, libepoxy*.dylib, libEGL.dylib and libGLESv2.dylib all exist.
- Emit `rustc-link-search=native=<prefix>/lib`.
- Emit `rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks`, then `-Wl,-rpath,<prefix>/lib`.
- Emit rerun-if-env-changed and rerun-if-changed on the prefix.
- The BERTH_AGENT and BERTH_GUI_MAC logic is unchanged.

**2. Cargo.toml.** Comment the `virgl` feature.

**3. build.sh.**
- `BERTH_VIRGL=auto|1|0` (default auto). auto turns the feature on when the prefix's libvirglrenderer exists; 1 forces it and errors when the libs are missing.
- Print `berth: virgl on|off`.
- Pass the features only to `cargo build --release -p berth`.

**4. src/main.rs.** Hidden `berth gpu-probe [--json]`.
- With the feature: call `gpu::virgl::probe()`, or `ffi::Renderer::get().capsets()` until A lands. Print `capset <id> <name> v<max> <bytes>` lines and the GL renderer string when available, and exit 0.
- Without the feature: print `virgl: not built` and exit 2.
- It needs no daemon.

**5. build-app.sh.** If `otool -L target/release/berth | grep @rpath/libvirglrenderer`:
- mkdir Contents/Frameworks;
- copy the recursive @rpath closure (`otool -L`), plus libEGL and libGLESv2 explicitly because libepoxy dlopens them, with `cp -L` under their install-name leaf names;
- add `-add_rpath @loader_path` to libepoxy and libvirglrenderer when missing;
- `install_name_tool -delete_rpath <abs prefix>/lib` on Contents/Helpers/berth;
- `codesign --force --sign - --timestamp=none` each dylib, then the inner berth with entitlements, then the bundle.

Without virgl, remove any stale Frameworks folder.

**6. sign.sh.** Unchanged, except a comment that `--options runtime` would need disable-library-validation or same-team signing.

**7. check-bundle.sh [app].** Exits non-zero when:
- any Mach-O references an absolute target/ path or LC_RPATH;
- an @rpath dependency is missing from Frameworks;
- `codesign --verify --deep --strict` fails;
- `DYLD_PRINT_LIBRARIES=1 Contents/Helpers/berth gpu-probe` loads any of the four libs from outside the bundle, or prints no capset 2.

A bundle without virgl passes with the note 'no virgl'.

**8. Docs.** Document BERTH_VIRGL, BERTH_VIRGL_PREFIX, gpu_virgl, gpu_blob and BERTH_GPU_VIRGL=0, and update the README S2 note.

**Done when (host).** With the prefix present:
- `scripts/build.sh` prints `virgl on`;
- `otool -L target/release/berth` lists @rpath/libvirglrenderer and @rpath/libEGL;
- `otool -l` shows LC_RPATH @executable_path/../Frameworks and the prefix;
- `target/release/berth gpu-probe` prints capset 1 and capset 2;
- `cargo test -p berth --features virgl gpu::virgl -- --include-ignored` works with no RUSTFLAGS.

`BERTH_VIRGL=0 scripts/build.sh` prints `virgl off`, and gpu-probe exits 2.

`scripts/build-app.sh && scripts/check-bundle.sh target/Berth.app` passes.

With target/spikes/gpu/prefix temporarily renamed, the bundled `Contents/Helpers/berth gpu-probe` still prints VIRGL2, loading the libs from Contents/Frameworks. Restore the prefix afterwards.

A bundle built with BERTH_VIRGL=0 passes check-bundle.

**Live check.** Install the new Berth.app and start berth from it:
- the daemon log shows capsets VIRGL and VIRGL2;
- `vmmap <daemon pid> | grep libGLESv2` shows the bundle path;
- there are no dyld errors.

### C-surfaces-display: Surface registry with shared-memory rings, the daemon `display` stream (doorbell plus input), session feeders and a fast screenshot

Depends on: nothing

Files: `src/gpu/frames.rs`, `src/gpu/ring.rs (new)`, `src/gui/display.rs (new)`, `src/gui/mod.rs`, `src/daemon.rs`, `docs/display-protocol.md (new)`, `docs/gui-protocol.md (link only)`

**Work.** **1. ring.rs.**
- `Ring::create(dir, key, gen, w, h)` uses the layout in the architecture: a 16 KiB header; stride = align_up(w*4, 256); slot_bytes = align_up(stride*h, 16384); 3 slots; a 256x256x4 cursor area.
- The file has mode 0600 and is created with libc open, ftruncate and mmap MAP_SHARED. Stale files are removed on create and on drop.
- `write_rect(rect, rows)` copies the undamaged area from the previous latest slot, then the seqlock goes odd, the data is written, and it goes even (Release ordering), then `latest` is published.
- `set_cursor`.
- `Ring::open_read(path)` checks magic and version, for tests and `once`.

**2. frames.rs.**
- `SurfaceKey` and `pub static SURFACES` (Mutex<HashMap> + Condvar).
- Each Surface holds {ring, seq, damage ring of 16 (seq, Rect), cursor + cursor_seq}.
- API:
  - `update(key, w, h, rect, draw)`, with the same closure shape; a size change makes a new gen;
  - `wait_newer(key, frame_seq, cursor_seq, timeout) -> Ev{Frame{slot, seq, damage_union | full}, Cursor, Resize, Timeout}`;
  - `info`, `png`, `set_cursor`/`move_cursor`, `remove`.
- `DISPLAY` remains a facade onto Scanout(n), keeping `update`/`latest`/`set_cursor`/`move_cursor`/`remove` so server.rs and Dump::tick are unchanged.
- Drop the dead_code allows.

**3. display.rs.** `serve(stream, DisplayReq{machine, session: Option, scanout: Option, max_fps: Option (60, clamped 1..=120), once})`.
- **Feeders:** `Mutex<HashMap<(machine, session), Feeder>>`, behind a `trait GuestConnector` (the real one calls `crate::daemon::guest_stream`) so tests can fake it.
  - The feeder sends `{"v":1,"session":S,"verb":"screen.stream","args":{"format":"bgra8","max_fps":60}}` and reads the reply line.
  - Then it loops: read a record line, read payload_len bytes, and call `SURFACES.update` once per damage rect.
  - A `res_id` record goes through `static RESOURCE_READER: OnceLock<fn(u32, Rect, &mut [u8], usize) -> Result<()>>`; when that is unset, subscribers get an `end` error.
  - The feeder stops 10 s after the last subscriber.
- **Per subscriber:**
  - write the reply line;
  - a blocking pacing loop over wait_newer sends frame, resize and cursor doorbells, keeping only the newest within 1/max_fps, and sends `{"ev":"keepalive"}` after 1 s idle;
  - a reader parses input lines and validates them (x/y in range, evdev code < 0x300, known ev);
  - while control is on, valid lines go to the per-session `InputPipe`, which lazily opens guest_stream with `input.stream` and writes the lines verbatim;
  - control off and disconnect send release_all;
  - each control toggle writes one activity-log entry with who=berth-app, through src/gui/mod.rs's writer.
- `once:true`: if there is no live surface, reply `{ok:false, error:"no live surface"}` at once; otherwise reply `{ok, payload_len}` followed by the PNG.
- Scanout keys have no feeder.

**4. daemon.rs.** `Request::Display{machine, session, scanout, max_fps, #[serde(default)] once}`. handle_control (:466) returns early into `gui::display::serve`, gated on gui=true.

**5. src/gui/mod.rs (host CLI).**
- `berth gui screenshot` on Linux tries `Display{once:true}` first, then falls back to grim.
- Hidden `berth gui watch <target> [--session S | --scanout N] [--frames N]` prints fps and damage.
- `berth machine display <m>` is `watch --scanout 0` for now; G turns it into opening the window.

**6. docs/display-protocol.md.** Byte offsets of the ring, the seqlock rule, gens, the doorbell, input lines, `transports`, and the agent record format (shared with D) exactly as in the architecture.

**Done when (host).** `cargo test -p berth` passes new tests:
- ring layout: header 16384, slot offsets %16384 == 0, stride %256 == 0 for widths 640, 1366, 1920 and 7680;
- seqlock: concurrent writer and reader over 10k frames, and a reader never accepts a torn slot;
- write_rect keeps the undamaged pixels;
- a resize makes a new gen and removes the old file;
- SURFACES keys are independent;
- wait_newer times out and wakes per key;
- the damage union over skipped frames, with a full frame after more than 16 or after a resize;
- cursor events;
- the DISPLAY facade feeds Scanout(0), and the existing frames/png tests still pass;
- the feeder parser on a fixture of 3 records (full, 2-rect damage, res_id with the reader unset) gives the expected ring pixels and an end error;
- end to end over UnixStream::pair with a fake GuestConnector:
  - the reply line arrives, open_read maps the ring, and frame events point at slots holding the right pixels;
  - max_fps=10 throttles a 100 Hz producer to ≤11 events/s, the last one covering the union;
  - input is dropped while control is off and forwarded verbatim while it is on, with release_all on disconnect;
  - an invalid line is rejected;
  - `once` with no feeder answers 'no live surface' in under 10 ms;
- Request::Display serde round trip with defaults.

`cargo clippy -p berth` is clean.

**Live check.** With D deployed and a sway session open:
- `berth gui watch dev --session s-x` shows damage when a window moves, and about 0 fps when idle;
- `ls ~/.berth/run/surfaces` shows the ring;
- `berth gui screenshot` while watching returns in under 50 ms.

`berth gui watch dev --scanout 0` shows fbcon frames, and `cat /dev/urandom > /dev/fb0` in the VM produces frames, with no A needed. With A it shows kmscube.

### D-agent-streams: Agent `screen.stream` and `input.stream`: wlr-screencopy over wl_shm with damage; a long-lived virtual pointer and keyboard

Depends on: nothing

Files: `agent/src/gui/wayland.rs`, `agent/src/gui/capture.rs (new)`, `agent/src/gui/input.rs (new)`, `agent/src/gui/keymap-us.xkb (new)`, `agent/src/gui/exec.rs`, `agent/src/gui/mod.rs (serve/dispatch/handle_stream only)`, `agent/src/main.rs (gui-capture, gui-input subcommands)`, `docs/gui-protocol.md`

**Work.** **1. wayland.rs: generalise `Conn`.**
- `send_with_fd` uses sendmsg with SCM_RIGHTS.
- `Arg::Fd` is a placeholder that is not encoded inline.
- `bind_globals(&[(name, ver)])`.
- wl_array decode.
- `memfd(name, len)`: memfd_create on Linux, an unlinked tempfile under cfg(not linux) so host tests run.
- The existing pointer plan keeps working.

**2. capture.rs: `run(out, fmt, max_fps)`.**
- Bind wl_shm, wl_output and zwlr_screencopy_manager_v1 (v ≥3).
- Per frame: capture_output(overlay_cursor=0) → the buffer event → reuse the memfd pool and wl_buffer (XRGB/ARGB8888 is BGRA in memory) → copy_with_damage → damage events → ready.
- Emit the record exactly per docs/display-protocol.md:
  - the first frame is a full frame;
  - honour y_invert;
  - `failed` re-arms after 100 ms;
  - pace to max_fps.
- `format:"resource"` answers `{"error":"not implemented"}`. The virtgpu ioctl structs (RESOURCE_CREATE 0xC0386444, RESOURCE_INFO, PRIME_HANDLE_TO_FD) are declared and size-pinned, for the post-S3 path.

**3. input.rs: `run(stdin, extent)`.**
- One zwlr_virtual_pointer_v1 and one zwp_virtual_keyboard_v1. The keymap is `include_str!("keymap-us.xkb")`, a checked-in output of `xkbcli compile-keymap --layout us`, NUL-terminated and sent as a memfd with format 1.
- It reads the input lines from docs/display-protocol.md:
  - pointer → motion_absolute + frame;
  - button → button + frame;
  - scroll → axis/axis_discrete + frame;
  - key → key plus modifiers from a tracked mask (42/54 shift, 29/97 ctrl, 56/100 alt, 125/126 super);
  - release_all.
- EOF releases everything held.

**4. agent main.rs.** Subcommands `gui-capture <json>` (stdout carries records) and `gui-input <w>x<h>` (stdin carries events), run as `$RT/berth-gui-input`.

**5. exec.rs.** `Machine::spawn_piped(unit, script) -> Child`: `systemd-run -M --uid --pipe --quiet --wait --unit berth-gui-<id>-cap|inp`, or the nsenter+su fallback. Dropping it kills the child and stops the unit.

**6. mod.rs.**
- `dispatch` (247-283) gets the verbs screen.stream and input.stream.
- `handle_stream` (127) gets a streaming path: write the reply line `{ok:true, payload_len:0}`, then pump (child stdout → vsock, or vsock → child stdin).
- It ends on either side closing.
- A dead session (prelude exit 3) gives a final `{"error":"session ended"}` record.
- The input extent comes from Session.size.

**7. docs/gui-protocol.md.** A 'Streams' section covering the exception to one request per connection, the record format, the input lines and the lifetime.

**Done when (host).** `cargo test -p berth-agent` on the host passes new tests:
- wire bytes for wl_shm.create_pool/create_buffer, capture_output, copy_with_damage, keyboard keymap/key/modifiers, and pointer motion_absolute/button/axis_discrete/frame all match hand-assembled bytes;
- `send_with_fd` over UnixStream::pair delivers the bytes plus a usable fd (write through it, read back);
- the capture record writer: an 8x4 frame with damage [[1,1,2,2]] gives the exact JSON line and packed payload; the first frame is full; y_invert flips the rows;
- input lines map to the expected messages; the modifier mask tracks shift and ctrl; EOF releases held keys and buttons; an unknown `ev` is rejected;
- the virtgpu ioctl numbers and sizes are pinned;
- the keymap has its 4 sections and is sent NUL-terminated.

`cargo build --release -p berth-agent --target aarch64-unknown-linux-musl` builds.

**Live check.** Deploy the agent and open a sway session. Run `berth-agent gui-local dev '{"v":1,"session":"s-x","verb":"screen.stream","args":{}}' | head -c 300`: the reply is followed by a full-size record, and moving a window gives small damage records.

input.stream:
- a click lands, the same as `berth gui click`;
- shifted typing works in foot;
- the same works on cage.

Measure the vsock throughput of full 1080p frames, and the fps for a full-screen damage, and record both in docs/LEARNINGS.md.

### E-gpu-sessions-desktop: GLES sway/cage sessions, render node and card0 binds retrofit, Mesa packages, and the `desktop` kind (sway-on-DRM and Hyprland/Omarchy) with seatd

Depends on: nothing

Files: `agent/src/gui/install.rs`, `agent/src/gui/hypr.rs (new)`, `agent/src/gui/exec.rs (spawn_root only, appended after D)`, `agent/src/machines.rs`, `agent/src/gui/mod.rs (session_new, launcher, prelude, kind dispatch, windows/open/focus/quit only)`, `docs/gui.md`

**Work.** **1. install.rs.**
- `packages` (78-121) adds Mesa explicitly:
  - Debian: libegl-mesa0, libgbm1, libgles2, libgl1-mesa-dri|mesa-libgallium, seatd, mesa-utils, wayland-utils, weston, glmark2-es2-wayland/glmark2-es2-drm where available;
  - Fedora: mesa-dri-drivers, mesa-libEGL, mesa-libgbm, seatd, mesa-demos, weston, glmark2;
  - Arch: mesa, mesa-utils, wayland-utils, seatd, weston, glmark2.
  Mark the unverified names, and when a package is missing, skip it with a warning rather than fail the install.
- `OMARCHY_CORE` (125) adds seatd.
- `gpu_binds(&GpuNodes{render, card})` (188) returns one `Bind=` line per present node, plus a drop-in `[Service]\nDeviceAllow=/dev/dri/renderD128 rw\nDeviceAllow=/dev/dri/card0 rw`.
- card0 is bound for every GUI machine, so the sway desktop works off Omarchy.
- Add `card_node_present()`.
- Add `desktop_ready(flavour, home)`:
  - hyprland needs Hyprland on PATH and ~/.config/hypr; uwsm is optional;
  - sway needs sway.
- Update `missing_tools_message` (446).

**2. machines.rs.**
- `mounts()` (121-130) filters both /dev/dri binds.
- Extract `ensure_gpu_binds(name) -> Result<bool>` from create (253-263). It is idempotent on the .nspawn file and the drop-in.
- Call it from create, `setup_gui` (303) and `integrate_all` (536) for machines with the GUI marker.
- When something changed: `systemctl daemon-reload`, plus a restart only when the machine is running, logged `gui: <name>: GPU nodes bound, restarting`.
- Delete and rename are unchanged.

**3. exec.rs.** `Machine::spawn_root(unit, script)`, used to start `seatd -g seat` (or `-u <user>`) once per machine with SEATD_VTBOUND=0 and socket /run/seatd.sock. The setup also adds the user to group seat (`groupadd -f seat; usermod -aG seat`).

**4. mod.rs launcher (424-444).**
- The gles2 branch adds WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128.
- Add a `renderer` parameter. session_new retries once with pixman when gles2 fails readiness (496-503), and logs the journal tail.
- `launcher_desktop(id, flavour)`:
  - sway: WLR_BACKENDS=drm, WLR_DRM_DEVICES=/dev/dri/card0, WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128, WLR_LIBINPUT_NO_DEVICES=1, LIBSEAT_BACKEND=seatd, `exec sway -c $RT/sway.conf`;
  - hyprland: AQ_DRM_DEVICES=/dev/dri/card0, LIBSEAT_BACKEND=seatd, XDG_SESSION_TYPE=wayland, XDG_CURRENT_DESKTOP=Hyprland, then `exec uwsm start -- hyprland.desktop` when ~/.local/share/omarchy and uwsm exist, else `exec Hyprland`;
  - neither sets WLR_BACKENDS=headless;
  - both keep berth's private dbus-daemon so AT-SPI works.

**5. session_new (464).**
- Accept `args.kind` (agent|app|desktop; the default is inferred as today) and `args.flavour` (hyprland|sway; auto picks hyprland when desktop_ready(hyprland)).
- desktop requires card0 and desktop_ready. A second desktop in the VM is refused with the existing session named; check it across machines through a VM-wide lock file /run/berth-gui-desktop.
- Skip sway.conf only for hyprland.
- Readiness waits for $RT/hypr/*/.socket.sock (hyprland) or the sway IPC socket.
- The reply and session.list add `kind`, `renderer` (gles2|pixman) and `scanout:0` for desktop.

**6. prelude (400-411).** Export HYPRLAND_INSTANCE_SIGNATURE from $RT/hypr/*.

**7. Kind dispatch.** `Ctx::sway()` becomes `enum Kind{Agent, App, Desktop(flavour)}`, matched in windows (632), open (672; hyprland diffs the `hyprctl clients -j` addresses), focus (888: `hyprctl dispatch focuswindow address:0x…`), quit (899: `closewindow`) and tree. Desktop sway uses the sway paths.

**8. hypr.rs.** `windows(json) -> Vec<sway::Window>`, reading address, pid, at, size, class, title, focusHistoryID==0 as focused, hidden/mapped and workspace. Plus `focus_cmd` and `close_cmd`.

**9.** Update `launchers_quote_apps` (1035-1042), and document the kinds and limits in docs/gui.md.

**Done when (host).** `cargo test -p berth-agent` passes:
- gpu_binds for {}, {render} and {render, card} gives the exact lines and drop-in;
- ensure_gpu_binds on a temp .nspawn is idempotent (the second run reports no change) and keeps unrelated lines;
- mounts() hides both binds;
- the gles2 launcher has WLR_RENDER_DRM_DEVICE and the pixman one doesn't;
- launcher_desktop(sway) has WLR_BACKENDS=drm and WLR_DRM_DEVICES;
- launcher_desktop(hyprland), with and without Omarchy, uses uwsm or Hyprland plus AQ_DRM_DEVICES and LIBSEAT_BACKEND, with no WLR_BACKENDS=headless; quoting is extended;
- arg parsing: desktop is refused when desktop_ready fails (fake home), a second desktop is refused, and the flavour is picked automatically;
- the readiness predicate for each kind;
- hypr::windows parses a checked-in fixture (one focused window, one hidden);
- the focus and close strings;
- the package lists include Mesa and seatd on each distro.

`cargo build --release -p berth-agent --target aarch64-unknown-linux-musl` builds.

**Live check.** With A live:
- `berth gui session new dev` reports renderer gles2, and weston-simple-egl runs. With virgl off it falls back to pixman.
- A machine created before this change gets its binds from integrate_all, with one restart.
- `--kind desktop --flavour sway` takes DRM master on card0 through seatd, and the scanout dump shows sway.
- On a `--gui=omarchy` machine, `--kind desktop` starts Hyprland through uwsm with Omarchy's bar visible in the scanout. `windows`, `focus` and `quit` work, and grim screenshots still work.
- Also try the desktop without virgl to learn whether kms_swrast is enough, and record the result in LEARNINGS.

### F-app-metal-display: Berth.app live Display: SurfaceStream from the ring, CAMetalLayer paced by the display link, Take control input, a ring-backed peek

Depends on: nothing

Files: `app/Package.swift`, `app/Sources/Berth/SurfaceStream.swift (new)`, `app/Sources/Berth/MetalFrameView.swift (new)`, `app/Sources/Berth/GuestInput.swift (new)`, `app/Sources/Berth/Display.swift`, `app/Sources/Berth/UnixSocket.swift`, `app/Sources/Berth/Engine.swift`, `app/Sources/Berth/Agents.swift`, `app/Tests/BerthTests/*.swift (new)`

**Work.** **1. UnixSocket.swift.** `readLine()` (buffered), `readExactly(n)`, `writeLine(json)`, and a `LineStream` with a reader thread, an AsyncStream of lines, and `cancel()` that does shutdown and close. Model it on UnixHTTP.stream.

**2. Engine.swift.** `Control.open(_ obj:) -> LineStream`, which sends a full JSON object. `Control.request` (:80) stays as it is.

**3. SurfaceStream.swift.**
- `init(target: DisplayTarget, source: .session(String) | .scanout(Int), maxFPS, socketPath injectable)`.
- It sends the display request and parses RingInfo.
- `Ring` maps the file with mmap PROT_READ / MAP_SHARED and validates magic 'BRTHSURF' and v1. It exposes latest, slotSeq, slotOffset, stride and the cursor fields. `makeBuffer(device)` uses bytesNoCopy with no deallocator, and munmap happens on deinit after the GPU completes.
- Doorbells update an atomic pending (slot, seq). resize remaps the new gen. end closes the stream.
- `send(InputEvent)`.
- Reconnect with backoff. After 3 failures it reports `.unavailable`, so Display falls back to screenshots.

**4. MetalFrameView.swift.**
- An NSView with a CAMetalLayer: bgra8Unorm, framebufferOnly, maximumDrawableCount 3, contentsScale from the window, nearest filtering at integer scale and linear otherwise.
- A `displayLink(target:selector:)` callback (macOS 14):
  - if pending.seq > drawn, blit the linear texture at slotOffset (bytesPerRow = stride) into a private texture;
  - on completion, recheck slotSeq and keep the old texture when torn;
  - draw an aspect-fit quad plus the cursor sprite into `nextDrawable()` and present.
- It pauses when occluded or on Pause.

**5. GuestInput.swift.**
- `KeyMap.evdev(macKeyCode:)`: the full ANSI table, modifiers through flagsChanged, arrows, F1-F12 and keypad.
- `guestPoint(view, viewSize, guestSize, flipped) -> (Int, Int)?`, which returns nil in the letterbox.
- Scroll accumulates precise deltas into discrete steps of 15 px.
- In MetalFrameView (`acceptsFirstResponder`): keyDown/keyUp/flagsChanged/performKeyEquivalent, consumed while controlling (Cmd-Q and Cmd-W pass through); mouse down/up/dragged/moved through a tracking area; scrollWheel.
- Events are sent only while `controlling`. The toggle sends control on/off, and turning control off sends release_all. Ctrl-Opt-Enter releases control.

**6. Display.swift.**
- Add `case stream` to DisplayBackend (:23), the default for Linux targets.
- The source is `.scanout(0)` when the session JSON has `scanout`, else `.session`.
- `DisplayFrames.run` (:110) falls back to `.screenshots(every: 2s)` when the stream is unavailable.
- FrameView (:197) uses MetalFrameView for stream and keeps the CGImage layer for the fallback.
- Toolbar: Take control, Pause and Refresh. The status text (:166) shows fps over 1 s and GPU/CPU from `renderer`.

**7. Agents.swift.** SessionPeek (701-760) uses a SurfaceStream at 2 fps, turning the latest slot into a CGImage through CGDataProvider with no Metal, and falls back to DisplayCapture.shoot. It never sends input.

**8. Package.swift.** `.testTarget(name: "BerthTests", dependencies: ["Berth"], path: "Tests/BerthTests")`.

**Done when (host).** `swift build --package-path app` builds (with `--build-system native` when there is no Metal Toolchain), with no CVDisplayLink deprecation warnings.

`swift test --package-path app` passes:
- **RingTests:** a fixture ring written per docs/display-protocol.md (1920x1080, stride 7680) parses; offsets and seqs are right; bad magic or version is refused.
- **MetalTests** (skipped when there is no device): bytesNoCopy over the mmapped fixture works; a linear texture at slot 1 reads back the pattern; the torn recheck rejects a changed seq.
- **SurfaceStreamTests:** a fake server on a temp socket answers the reply, then 3 frame events and a resize. The stream reaches pending seq 3, then remaps the gen. Input lines from send arrive verbatim. A server close leads to reconnect, and to `.unavailable` after 3 failures.
- **GuestInputTests:** kVK_ANSI_A→30, Return→28, Shift→42, Command→125, LeftArrow→105, F1→59. In a 1000x1000 view showing 1920x1080, the letterbox maps the corner and centre correctly and the bars give nil. The y-flip is right. One discrete scroll step per 15 px.
- The display request JSON and every input event match the protocol doc strings.

**Live check.** Open the Display window for a sway session:
- it updates smoothly while dragging a guest window, and idles with near-zero CPU when the screen is static;
- with Take control, clicking and typing work in foot and Firefox, Cmd shortcuts don't fire the app menus, and the activity log shows berth-app control entries;
- the Agents peek still refreshes;
- with a daemon from before C, the window falls back to 2 s screenshots.

### G-wire-and-accept: Post-merge wiring and Phase 3/4 acceptance

Depends on: A-virgl-device, B-packaging, C-surfaces-display, D-agent-streams, E-gpu-sessions-desktop, F-app-metal-display

Files: `src/gui/display.rs (one line: RESOURCE_READER)`, `src/main.rs (gpu-probe switched to virgl::probe; `berth machine display` opens the window)`, `app/Sources/Berth/BerthApp.swift (open-display handling, if needed)`, `docs/LEARNINGS.md`, `docs/plans/gui-vms-agent-testing.md (status)`, `bench/spikes/gpu/README.md (S3 status)`

**Work.** 1. Set `display::RESOURCE_READER` from `gpu::device::read_resource`.
2. Point gpu-probe at `virgl::probe()`.
3. `berth machine display <m>`: start (or reuse) the desktop session and open Berth.app's Display window for scanout 0, using the app's existing open mechanism. Check how BerthApp.swift:51 `openWindow(id:"display")` can be reached from outside the app (a URL scheme or NSWorkspace with arguments), and add the smallest hook. If none fits, print the instructions.
4. Re-run S3 (s3-headless-dmabuf.sh) now that A and E exist, and record the result.
5. Acceptance runs, recorded in docs/LEARNINGS.md:
   - **Phase 3:** in a sway desktop on card0, `weston-simple-egl` and `glmark2-es2-wayland` report the virgl ANGLE renderer, score above LIBGL_ALWAYS_SOFTWARE=1, and show in the Display window. The status line shows about the display rate (60 or 120). Also measure the same apps in a headless gles2 session through screen.stream, and note the fps.
   - **Phase 4 (Omarchy part):** on a `--gui=omarchy` machine, the Hyprland desktop shows Omarchy's bar. Take control types into the terminal, moves windows and opens the launcher.
   - A cage app session (`berth gui session new dev --app firefox`) shows as one window.

**Done when (host).** - `cargo test -p berth` passes, as does `cargo test -p berth-agent`.
- `swift test --package-path app` passes.
- `scripts/build-app.sh && scripts/check-bundle.sh target/Berth.app` passes.
- A test shows that a `res_id` record with the reader set reaches FakeBackend.read_back.

**Live check.** The Phase 3 done-when as stated in the plan: weston-simple-egl and glmark2 run GPU-accelerated in a Mac window at display rate. Then the Omarchy desktop is usable with Take control. Record the numbers and anything that failed in LEARNINGS.

## Risks

- VZ element lifetime: keeping a VZVirtioQueueElement past `drain`, and writing or returning it later from a block dispatched onto the same queue, is unproven on macOS 27. Fenced 3D work and async MapBlob both depend on it. Prove it first in A with a FakeBackend deferral on the live VM. If it fails, the fallback is to block in drain, polling through the worker until the fence signals: correct, but it serialises the guest.
- Thread confinement: every virgl and EGL call must go through the berth-virgl worker, including `Renderer::get()`/`capsets()`. One direct call from the GCD queue's thread brings back EGL_BAD_ACCESS or GL calls running with no current context. `virgl::probe()` must start its own worker.
- THREAD_SYNC on macOS: virglrenderer's sync thread uses eventfd on Linux, so `get_poll_fd` may return -1 or behave differently on macOS. The 1 ms tick covers that, but fence latency must be measured. Fence IDs: virglrenderer's global fences are 32-bit, so A keeps its own u32 sequence mapped to the guest's u64.
- Saved state: whether returning None from save_state makes the VM save fail cleanly, so snapshots.rs falls back to disk rather than erroring, is unverified. Users with a live 3D context lose fast resume.
- Scanout readback cost: transfer_read_iov of a full 1080p frame through ANGLE/Metal on every RESOURCE_FLUSH may cap the desktop below display rate. If it does, bring the IOSurface pbuffer blit forward (virgl.rs:29-32), which is out of scope here.
- vsock bandwidth: full-frame BGRA over vsock (about 500 MB/s at 1080p60) is untested, so headless sessions running glmark2 may not reach display rate. Phase 3 is therefore proven on the card0 desktop path, and headless GPU sessions reach full rate only with the post-S3 res_id path.
- KMS inside nspawn (unverified): seatd with SEATD_VTBOUND=0 and no VT; DRM master for a non-root user while fbcon holds card0; DeviceAllow for card0; wlroots and Aquamarine accepting card0 for KMS with rendering on renderD128 (same driver, so same GPU); Hyprland with no libinput devices. Only one desktop per VM.
- Hyprland and Omarchy support for zwlr_virtual_pointer_v1, zwp_virtual_keyboard_v1 and wlr-screencopy is unverified. If any is missing, input would need a custom virtio-input device, which is out of scope.
- The render node's existence: S3 says renderD128 appears only with virgl, while S1 saw it without. ensure_gpu_binds must also run once the node appears later, through integrate_all.
- Mesa driver names and package names per distro and arch (Debian --no-install-recommends; glmark2 and weston on arm64) are partly unverified. E skips a missing optional package with a warning.
- Packaging: libepoxy's patched dlopen of @rpath/libEGL must resolve inside Frameworks (through @loader_path or the executable's rpath). Ad-hoc dylibs work only while there is no hardened runtime.
- Merge order: D and E both edit agent/src/gui/mod.rs (in disjoint functions) and exec.rs (append-only), so merge D before E. The display-protocol contract must not drift: C owns the document, and D and F test against the strings copied in this plan.

## Out of scope

- Zero-copy presentation: an IOSurface pbuffer through EGL_ANGLE_iosurface_client_buffer, IOSurface handoff over XPC or Mach, and an SMAppService GPU broker. `transports` leaves room for these.
- Headless dmabuf capture sending virtio-gpu resource IDs (the post-S3 res_id path): the record format, the RESOURCE_READER hook and the ioctl layouts are reserved, but the allocation is not built.
- Live use of Venus/Vulkan, MoltenVK and blob resources: only the hooks are in scope (capset passthrough, CONTEXT_INIT, ring_idx fences, MapBlob through the shared memory region, all behind gpu_blob=false).
- A custom virtio-input device (ID 18), pointer lock and relative motion, and CGEventTap capture of system shortcuts.
- Multiple scanouts, resizing the display without a reboot (events_read stays 0), and HiDPI scale negotiation.
- One Mac window per toplevel for `berth machine run dev -- firefox` (Phase 6). Here one cage session is one window, and the CLI-to-window hook is best-effort in G.
- Clipboard sync, audio, and the game session kind.
- Notarisation, hardened runtime, Developer ID signing, and CI caching or reproducible builds of target/spikes/gpu/prefix.
- Moving `wait idle` to damage events (it can use screen.stream later).
