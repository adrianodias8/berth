# GPU and GUI spikes (Phase 0)

Plan: `docs/plans/gui-vms-agent-testing.md`. Each spike answers one question
the display foundation (`src/gpu`) or macOS VMs depend on. Record every
result, pass or fail, with numbers and the date, in `docs/LEARNINGS.md`.

None of the scripts start, stop or restart berth; where a spike needs a
restart, the steps say so. Throwaway machines are named `gui-test-*` and
deleted at the end.

| Spike | Question | Script | Needs |
|---|---|---|---|
| S1 | Does a custom virtio-gpu (ID 16) bind the guest's stock `virtio_gpu`, and does what it draws reach the Mac? | `s1-bind.sh` | macOS 27, berth's kernel rebuilt with the new config |
| S2 | Does virglrenderer run on ANGLE's Metal backend? | `s2-virgl-angle.sh` | Xcode, meson, ninja, ~10 GB |
| S3 | Can a headless compositor on `renderD128` hand frames out as dma-bufs? | `s3-headless-dmabuf.sh` | S1, S2 and the 3D path |
| S4 | Can host memory (an MTLBuffer's pages) be mapped into the guest through the shared memory region? | `s4-s6-guest.sh s4` | S1 |
| S5 | Do macOS provisioning and TCC grants survive APFS cloning? | steps below | `berth mac` (Phase 1) |
| S6 | What happens to the guest's config-space writes? | `s4-s6-guest.sh s6` | S1 |

## S1: the device binds

1. Build berth's kernel with the display options
   (`guest/kernel/config.fragment`), and berth:

   ```sh
   guest/kernel/build.sh            # target/kernel; ~4 min
   bench/spikes/gpu/s1-bind.sh kernel   # every option is =y in target/kernel/config
   ./scripts/build.sh               # or ./scripts/build-app.sh, which bundles the kernel
   ```

   A development build (`target/release/berth`) boots `target/kernel`;
   the app boots the one bundled in it (`src/vm.rs`, `bundled_kernel`).
   Debian's kernel works too (`berth config set kernel debian`): it has
   `virtio_gpu` as a module.

2. Turn the display on. `berth config set` doesn't know the key yet, so
   edit `~/.berth/config.json` and add `"gui": true` (optional:
   `"gpu_display": "2560x1600"`, `"gpu_memory_mib": 256`):

   ```sh
   python3 -c 'import json,os; p=os.path.expanduser("~/.berth/config.json"); c=json.load(open(p)) if os.path.exists(p) else {}; c["gui"]=True; json.dump(c,open(p,"w"),indent=2)'
   ```

3. Restart berth yourself (`berth restart`, or quit and reopen the app).
   A saved state from before won't resume with the new device; berth then
   boots normally.

4. Check:

   ```sh
   bench/spikes/gpu/s1-bind.sh
   ```

**Pass:** every line `PASS`: the daemon log shows `gpu: device created` and
`gpu: driver ready, features edid, resource-uuid`; in the guest
`virtio_gpu` is bound to the ID-16 device, `/dev/dri/card0` exists, the
first mode is the configured size (from our EDID), dmesg's `[drm] features`
line has `+edid`; on the Mac `~/.berth/gpu/scanout-0.png` appears at the
configured size with more than half its pixels lit after the script writes
random bytes to `/dev/fb0`.

To undo: remove `"gui"` from config.json and restart berth.

## S2: virgl on ANGLE (Metal)

```sh
brew install meson ninja pkg-config
bench/spikes/gpu/s2-virgl-angle.sh
```

Builds ANGLE (Metal only), libepoxy with EGL, and virglrenderer (EGL,
no GBM) into `target/spikes/gpu`, then runs `probe_renderer`
(`src/gpu/virgl.rs`, feature `virgl`), which creates an ANGLE display on
Metal, starts virglrenderer on it through the `get_egl_display` callback,
and reads its capsets.

**Pass:** the test passes and prints `capset 2 max_version >= 1` (VIRGL2).
virglrenderer isn't built for macOS upstream; note every patch it needs.

## S3: headless dma-buf capture

```sh
bench/spikes/gpu/s3-headless-dmabuf.sh
```

Creates `gui-test-s3`, runs sway headless on `/dev/dri/renderD128` and
captures with grim. **Pass:** `/dev/dri/renderD128` exists, the compositor
offers `zwp_linux_dmabuf_v1`, sway doesn't fall back to llvmpipe, grim
captures a frame through a dma-buf. Blocked until the 3D path exists (the
driver only creates a render node with virgl) and machines bind the node
(Phase 4); the script says which.

## S4: host memory in the shared region

1. `mkdir -p ~/.berth/gpu && touch ~/.berth/gpu/spike-s4`, restart berth.
   When the guest driver starts, the device maps 2 MiB of host memory
   filled with `berth-s4` at offset 0 of the host-visible region
   (`src/gpu/device.rs`, `spike_s4`).
2. `bench/spikes/gpu/s4-s6-guest.sh s4`
3. `rm ~/.berth/gpu/spike-s4`, restart berth.

**Pass:** the daemon log has `gpu: S4: mapped 2 MiB … in <time>` (record
the time) and the guest reads `berth-s4` at the start of the region's BAR.
An MTLBuffer made with `newBufferWithBytesNoCopy` over the same pages is
then visible to the guest the same way.

## S5: macOS provisioning and TCC survive clones

With `berth mac` (Phase 1); nothing here downloads an IPSW by itself.

```sh
berth mac create s5-base                      # provisions a user, auto-login, SSH
berth mac shell s5-base -- tccutil reset All  # start from no grants
# grant berth-gui-mac Accessibility and Screen Recording once (the plan's base-image step)
berth mac stop s5-base
berth gui session new mac:s5-base --clone     # an APFS clone of the base
berth gui hello / screenshot / tree on the clone
berth mac delete <the clone>; berth mac delete s5-base
```

**Pass:** the clone boots to a logged-in desktop with no Setup Assistant in
under 20 s, and `screenshot` and `tree` work in it without a TCC prompt.
If a grant doesn't survive, record which (TCC grants are keyed to the
code signature and the boot volume).

## S6: config-space writes

```sh
bench/spikes/gpu/s4-s6-guest.sh s6
```

Reads the device's config space through its BAR, writes
`events_clear = 1` as the driver does when it acknowledges a display
event, and reads it back. **Pass:** the script runs and prints a `RESULT`
line; record it. The custom Virtio API gives the device no callback for
config writes either way, which is why `src/gpu/device.rs` never raises
`events_read`. If writes are stored, the next experiment is the host
raising `events_read` with `updateDeviceSpecificConfiguration` and
clearing it itself once the guest's GET_DISPLAY_INFO arrives: that would
make display resizes possible without a reset.
