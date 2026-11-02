# Guest OpenGL 3.3+ and Vulkan: plan

Status: plan, nothing built yet (2026-11-02). Inputs: the live game tests
(docs/LEARNINGS.md, "Games in the guest"), a research workflow over
virglrenderer, Mesa and ANGLE sources with caps probes on this Mac, and a
read of [fxgl/steamac](https://github.com/fxgl/steamac), which runs Valve's
ARM64 SteamOS with Proton on Apple Silicon through libkrun + Venus.

## Where we are

Guest (Arch Linux ARM, kernel 7.2.12-berth, Mesa 26.2 virgl) on host vrend
1.1.1 over ANGLE chromium/7258 Metal (ES 3.0):

| | Guest gets | Why (measured caps, Mesa/vrend source) |
|---|---|---|
| Desktop GL | 2.1 compat, no core | Mesa wants NV_conditional_render for GL 3.0; vrend reports it only on desktop GL 3.0+ hosts (`vrend_renderer.c` FEAT gl/nv_conditional_render). On any GLES host: 0. |
| GLES | 2.0 | ANGLE Metal has 12 vertex UBOs; vrend reports 12, Mesa virgl reserves one (11) and st/mesa turns UBOs off below 12; ES 3.0 needs them. |
| GLSL | 130 | vrend on a GLES host: 130 for ES 3.0, 310 for 3.1, 400+ only with tessellation + geometry shaders + gpu_shader5. Desktop GL 3.3 needs 330. ANGLE Metal is hard-capped at ES 3.0 (`mtl_common.h` kMaxSupportedGLVersion). |
| Vulkan | no device | Venus isn't built (`-Dvenus=false`), capsets are VIRGL/VIRGL2 only. |

**SuperTuxKart's black race.** berth's Hyprland launcher exports
`MESA_GLES_VERSION_OVERRIDE=3.0` (Hyprland needs ES 3), and everything the
desktop launches inherits it. STK then takes its ES 3 renderer, which keeps
its camera matrices and lighting in uniform blocks; Mesa still has
ARB_uniform_buffer_object off and rejects `GL_UNIFORM_BUFFER`, so the 3D
scene is black and only the HUD draws. Without the override STK's ES 2
renderer draws the race correctly at ~50 fps. (UBO mechanism traced in
source, not yet confirmed live: spike S8.)

## Routes

| Route | Guest GL | Guest Vulkan | Cost | Main risk |
|---|---|---|---|---|
| A. Fix ANGLE path (UBOs 12 to 16, scoped override) | ES 3.0 real, GL 2.1 (3.0 with a conditional-render fudge) | none | days | none big; ANGLE rebuild is slow |
| B. vrend on Apple OpenGL 4.1 core (CGL) | predicted 4.1 core, 3.0 compat, ES 3.0; no compute/SSBO (no 4.3, no ES 3.1) | none | 1-2 weeks | Apple GL is deprecated; rendering bugs (UTM's fork needed ~500 lines of CGL workarounds) |
| C. Venus on MoltenVK / KosmicKrisp | via Zink: GL 2.1 honestly on stock drivers; 4.6 with steamac's driver patches | 1.3-1.4 minus Metal gaps | weeks | Virtualization.framework mapping thousands of host blobs into the guest (unproven) |
| D. llvmpipe in the guest | 4.5 / ES 3.2 on CPU | lavapipe | none | slow; a diagnostic, not a solution |

Not routes: ANGLE's Vulkan backend on MoltenVK under vrend (ES 3.1 at most:
guest GL ~3.1, worse than B for games); DRM native context (no host DRM on
macOS); gfxstream (possible fallback if Venus stalls, unverified for this
setup).

**What steamac proves (C), and what it took:**
- Host: virglrenderer from UTM's `macos-next` fork merged with upstream main
  (venus-protocol 1.1.3, which Mesa 26.3 speaks; berth's 1.1.1 is too old),
  `-Dvenus=true -Drender-server-mode=thread -Drender-server-worker=thread`,
  Venus only (`VENUS | NO_VIRGL`), 19 patches: host-visible memory as POSIX
  shm imported with VK_EXT_external_memory_host, freed shm unmapped only after
  the GPU is done, failed pipelines as placeholders instead of fatal errors.
- Drivers: UTM's MoltenVK fork (geometry shaders on mesh shaders,
  robustness2, transform feedback) plus ~43 patches, or KosmicKrisp (Metal 4,
  macOS 26+, ~1.6x faster in their tests) with geometry shader and transform
  feedback MRs.
- VMM (libkrun on Hypervisor.framework): F_BLOB_ALIGNMENT with 16 KiB, each
  blob's shm fd mmapped and `hv_vm_map`ped into an 8 GiB window on MAP_BLOB,
  synchronously; THREAD_SYNC for Venus fences; SET_SCANOUT_BLOB copying from
  the shm; RLIMIT_NOFILE raised (about four fds per allocation).
- Guest: Linux 7.2+ with a blob placement patch (16 KiB-aligned offsets),
  Mesa main with "venus: honor the virtio-gpu blob alignment" (!44783), GL
  through Zink only (`MESA_LOADER_DRIVER_OVERRIDE=zink`,
  `MESA_GL_VERSION_OVERRIDE=4.6`). American Truck Simulator runs on GL 4.6
  core through Zink; shader compiles stutter ~50-100 ms the first time.

The difference for berth: VZ has no `hv_vm_map`. The only way to put host
pages in the guest is the shared memory region's
`mapMemory:atOffset:size:`, which berth has code for (`src/gpu/device.rs`,
behind `gpu_blob`) but has never run live.

## Recommended order

### Phase 0: honest ES 3.0, no black games (days)

1. **Scope the override to Hyprland.** Keep `MESA_GLES_VERSION_OVERRIDE=3.0`
   for the compositor only: unset it in the uwsm-app stand-in and in the
   session prelude, so apps don't inherit it. Done when STK launched from
   Omarchy takes its ES 2 path and draws the race.
2. **Spike S8, confirm the STK cause** (an hour, guest only): STK with
   `MESA_DEBUG=1` under the override, then with
   `MESA_EXTENSION_OVERRIDE=+GL_ARB_uniform_buffer_object`; check Mesa 26.2's
   UBO threshold and whether Arch's STK is a GLES build.
3. **ANGLE UBOs 12 to 16** (ANGLE main already has 16): patch
   `mtl_common.h` in `s2-virgl-angle.sh`, rebuild. Done when the guest reports
   ES 3.0 without any override, Hyprland starts without it, and STK's ES 3
   renderer draws the race.
4. Optional, behind a flag: report conditional render on GLES hosts (guest
   GL 3.0 compat; conditional draws always draw).

### Phase 1: GL 4.1 core through vrend on CGL (1-2 weeks)

5. **Spike S7, CGL in the live VM.** A CGL context backend next to the
   EGL/ANGLE one in `src/gpu/virgl.rs` (`CGLChoosePixelFormat` with
   `kCGLOGLPVersion_GL4_Core`, shared contexts, `CGLSetCurrentContext`), init
   without `USE_GLES`, ANGLE kept selectable in config. Start with
   `transfer_read_iov` readback. Record guest glxinfo/eglinfo; run glmark2,
   STK, Luanti `opengl3`, Hyprland, a piglit quick subset. Pass: guest GL 3.3+
   core and the games render correctly.
6. Port UTM's CGL fixes (`utmapp/virglrenderer` macos: 7848a07c80,
   71a6741401) as rendering bugs show up; move the PBO readback
   (`virgl_readback.rs`) to a CGL context in vrend's share group.
7. Decide the default per machine: CGL if it holds up, ANGLE as fallback.

### Phase 2: Vulkan through Venus (weeks; only if S4 passes)

8. **Spike S4 for real: the go/no-go for Venus under VZ.** Turn `gpu_blob`
   on and check from the guest through the DRM ioctls
   (`VIRTGPU_RESOURCE_CREATE_BLOB`, `VIRTGPU_MAP`, mmap of the GEM handle), not
   sysfs BAR mmap (that fails with EINVAL under `CONFIG_IO_STRICT_DEVMEM`, as
   in S6). Then map, in turn: anonymous pages, `shm_open` + `MAP_SHARED`, an
   MTLBuffer (`newBufferWithBytesNoCopy`) written by a Metal blit, a MoltenVK
   `vkMapMemory` pointer. Measure coherence both ways, map/unmap cost in a
   loop (hundreds to thousands), and the largest region VZ accepts (256 MiB,
   1, 4, 8 GiB). Fail means Venus can't work in VZ the way steamac does it.
9. **Spike S9, Venus host bring-up** (parallel with S4): virglrenderer from
   UTM's `macos-next` + upstream main, steamac's virglrenderer patches (MIT),
   Venus in thread mode, against UTM's MoltenVK fork. Pass: capset 4 offered,
   a host vkr context allocates host-visible memory as shm.
10. Device: offer F_BLOB_ALIGNMENT (bit 5) with `blob_alignment = 16384` in
    the config space, pass the Venus capset through, map each blob's shm on
    MAP_BLOB (synchronously, as the guest expects) and unmap on UNMAP/unref
    only after the GPU is done, grow the region to 4-8 GiB, raise
    RLIMIT_NOFILE, SET_SCANOUT_BLOB from the shm for Venus-rendered scanouts.
    Decide whether vrend (for the desktop) and Venus run side by side (steamac
    never does; unverified) or Venus-only with a 2D path for fbcon.
11. Guest: the placement patch in berth's 7.2 kernel; Mesa with !44783 and
    venus-protocol 1.1.3 (Arch's or our own build). Validate with
    `vulkaninfo`, vkcube, vkmark, then a native Vulkan game.

### Phase 3: GL 4.6 through Zink on Venus (with Phase 2)

12. Measure Zink's honest GL version on our host driver before overriding
    anything; with UTM's MoltenVK fork or patched KosmicKrisp, follow
    steamac's environment (`MESA_LOADER_DRIVER_OVERRIDE=zink`, version
    overrides) per machine, not globally, and validate per game. If Zink on
    Venus beats Phase 1 on correctness, it can replace vrend for apps.

### Alongside

- **Hyprland's formats:** it advertises only XR24 over linux-dmabuf, so
  Wayland EGL apps get no alpha configs (EGL_BAD_MATCH). Until fixed, games on
  Omarchy go through XWayland (`SDL_VIDEODRIVER=x11`). Find why Hyprland
  narrows the list (likely the virtio-gpu primary plane's XRGB8888 only).
- **Sound:** no audio device in the VM; virtio-snd to CoreAudio (steamac has
  one) is its own plan.
- **x86 games:** Proton/DXVK on aarch64 needs FEX (steamac's route); out of
  scope until Vulkan works.

## Decisions to make

- After S8 and Phase 0: is GL 4.1 (Phase 1) worth it before Venus, or go
  straight to S4/S9? Phase 1 is cheaper and doesn't depend on VZ mapping;
  Phase 2 is the only way to Vulkan and, with Zink, to GL 4.6.
- Driver: MoltenVK (macOS 15+, UTM fork + patches) or KosmicKrisp (macOS 26+,
  faster, Mesa main + MRs). steamac defaults to KosmicKrisp where it can.
- How much of steamac's patch stack to carry (MIT virglrenderer patches are
  easy; MoltenVK/KosmicKrisp patches fall under Apache-2.0 / MIT with their
  authors' copyright).

## Sources

- Measured caps: vrend 1.1.1 init probes on ANGLE Metal and on CGL (OpenGL
  "4.1 Metal - 91.7", macOS 27.2): GLSL 130 vs 410, conditional render 0 vs 1,
  UBOs 12 vs 16.
- virglrenderer `src/vrend_renderer.c` (caps filling, FEAT table, GLSL
  levels); Mesa `src/gallium/drivers/virgl/virgl_screen.c`,
  `src/mesa/state_tracker/st_extensions.c`, `src/mesa/main/version.c`,
  `bufferobj.c`; ANGLE `src/libANGLE/renderer/metal/mtl_common.h`; STK
  `src/graphics/central_settings.cpp`, `data/shaders/header.txt`.
- Upstream macOS Venus in virglrenderer main: !1600, !1601, !1602, !1626,
  !1688 (fences, open); Mesa Venus blob alignment !44783; kernel
  VIRTIO_GPU_F_BLOB_ALIGNMENT (7.2).
- steamac: `host/virglrenderer/`, `host/moltenvk/`, `host/kosmickrisp/`,
  `host/libkrun/patches/0001,0007,0008,0009`,
  `guest/kernel/patches/0001-drm-virtio-align-host-visible-blob-placement.patch`,
  `guest/mesa/versions.env`, `guest/layer/usr/lib/environment.d/60-steamac-mesa.conf`.
