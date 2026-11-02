# Feature plan: GUI VMs and agent-driven app testing

Status: in progress on `feat/gui-vms-agent-testing`. The host ↔ guest
contract is `docs/gui-protocol.md`.

## Summary

berth gains three things that build on each other:

1. **macOS VMs** (`berth mac …`) for testing Mac apps in a clean, disposable
   guest.
2. **Linux GUI**: GPU-accelerated Linux desktops and apps (an Omarchy/Hyprland
   desktop, GTK/Qt/Electron apps, Firefox) shown in Mac windows, built on
   **one display foundation** that games will later use unchanged.
3. **Agent testing** (`berth gui …`): one interface coding agents use to push
   an app into a macOS or Linux guest, open it, take screenshots, click, type
   and read the accessibility tree. Every action is logged with the agent that
   ran it, and Berth.app shows a live "peek" of what each agent is doing.

Games (Vulkan, Proton, gamepads) aren't in this plan, but every Linux GUI
decision keeps games an addition on the same foundation, never a second
graphics stack (see Games readiness).

Inspiration: Flavio Copes' VM Peek / `testvm` (Tart + SSH) for the agent
workflow, and RiftVM for 3D in Linux guests over macOS 27's custom Virtio
devices. As with OrbStack (`docs/LEARNINGS.md`, ground rules), berth builds
from their public descriptions only and copies no code.

## Goals

- A macOS guest that agents can test in without taking over the Mac's
  screen, reset to clean in about a second.
- A Linux desktop (Omarchy as the reference) that feels native: GPU rendering,
  smooth scrolling and resizing at the display's refresh rate, Mac keyboard and
  trackpad, clipboard.
- One graphics path for every Linux GUI use: desktop, single apps, headless
  agent sessions, and later games.
- One agent interface (`berth gui`) with the same verbs on macOS and Linux.
- An agent never clicks in another agent's app: each gets its own screen.

## Non-goals (for this plan)

- Vulkan / Venus, Proton, DXVK, gamepads, game audio tuning (games phase).
- Windows guests. Intel Macs. Remote access to guest screens.

## Where it fits in today's berth

| Today | Relevance |
|---|---|
| `src/vm.rs` builds one `VZVirtualMachineConfiguration` (generic platform, Linux) | The GPU device is added here; a second builder is added for macOS guests |
| Linux machines are systemd-nspawn containers on the shared VM kernel (`agent/src/machines.rs`) | One GPU device per VM, shared by every machine through `/dev/dri` binds |
| Guest agent on vsock (`agent/src/main.rs`, one header line per connection) | The new `gui` requests ride the same channel; the macOS guest gets its own agent speaking the same protocol |
| Own kernel (`guest/kernel/config.fragment`) | Needs `DRM_VIRTIO_GPU`, `UDMABUF`, `INPUT_UINPUT` and friends built in |
| APFS-clone snapshots (`src/snapshots.rs`) | Instant per-agent clones and reset-to-clean for macOS VMs |
| Berth.app SwiftUI (`app/Sources/Berth`) | New Display window, macOS VMs section, and Agents (peek) section |

## Architecture

```
                         ┌───────────────────────── Mac ─────────────────────────┐
 agents ── berth gui ──▶ │ berth daemon ── activity log (~/.berth/logs/gui)       │
 (CLI / MCP)             │   ├─ macOS VM  (VZMacPlatform, Apple GPU, VZ view)      │
                         │   │     └─ berth-gui-mac agent (vsock): AX, CGEvent,    │
                         │   │        screencapture, open/push                     │
                         │   └─ Linux VM  (shared kernel, nspawn machines)        │
                         │         ├─ berth-gpu  ◀── custom Virtio device ──▶     │
                         │         │    host: virglrenderer → ANGLE → Metal       │
                         │         │          scanout/dmabuf → IOSurface → window │
                         │         └─ per machine: GUI sessions (Wayland)         │
                         │              compositor on /dev/dri/renderD128         │
                         │              berth-gui agent: frames, input, AT-SPI    │
                         └─────────────────────────────────────────────────────────┘
 Berth.app: Display windows · macOS VMs · Agents (live peek + timelines)
```

### 1. macOS VMs

- Own config builder (`src/macvm.rs`): `VZMacPlatformConfiguration`,
  `VZMacGraphicsDeviceConfiguration`, `VZMacOSBootLoader`, vsock, virtio-net,
  a shared folder for pushes.
- `berth mac create <name>` downloads the latest supported IPSW
  (`VZMacOSRestoreImage`), installs it, and uses macOS 27's
  `VZMacGuestProvisioningOptions` (set on `VZMacOSVirtualMachineStartOptions`
  with `setGuestProvisioningOptions:error:`: fullName, username, password,
  logsInAutomatically, enablesRemoteLogin) so Setup Assistant never appears.
- A provisioned VM becomes a base image; clones are APFS copy-on-write, so
  `berth mac reset <name>` returns to clean in about a second.
- The guest agent (`guest/mac-agent`, Swift) runs as a LaunchAgent in the
  user session, granted Accessibility and Screen Recording once in the base
  image, so every clone inherits it.
- Apple's license allows 2 macOS VMs running at once; berth enforces it and
  queues the rest.
- CLI: `berth mac create|list|start|stop|reset|delete|shell|info`.

### 2. Linux display foundation: `berth-gpu`

One custom Virtio device (`VZCustomVirtioDeviceConfiguration`) speaking the
**standard virtio-gpu protocol** (device ID 16), so the guest uses stock
`virtio_gpu` DRM and Mesa `virgl`.

- Host (`src/gpu/`): `controlq` + `cursorq`; virglrenderer → ANGLE (Metal);
  guest-backed resources through `GuestMemoryMapping`; blob resources and a
  host-visible shared memory region declared from day one (Venus hook);
  presentation as IOSurface/MTLTexture in a `CAMetalLayer`, paced to the
  display link; the host keeps the latest frame of every surface for
  screenshots and the peek.
- **Sessions, not one global screen**: every GUI session is a headless
  Wayland compositor on `/dev/dri/renderD128` (machines get
  `Bind=/dev/dri/renderD128` + `DeviceAllow=`). The in-guest agent captures
  outputs as dmabufs (`ext-image-copy-capture` / `wlr-screencopy`) and sends
  only resource IDs. Input goes over vsock to Wayland virtual-keyboard /
  virtual-pointer. Session kinds: desktop (Omarchy/Hyprland), app (`cage`
  with one app), agent (headless), game (later). Fallback: KMS scanout on
  `card0` for the primary session.
- Also: clipboard both ways, HiDPI, hardware cursor through `cursorq`,
  audio via `VZVirtioSoundDeviceConfiguration` + PipeWire.
- Machine flags: `berth machine create desk --distro archlinux --gui=omarchy`,
  `berth machine create dev --gui`, `berth machine display desk`,
  `berth machine run dev -- firefox`.

### 3. `berth gui` (macOS and Linux)

Same verbs everywhere: `push`, `open`, `screenshot`, `click <x,y|"label">`,
`type`, `key`, `scroll`, `drag`, `tree`, `wait`, `windows`, `focus`, `quit`,
`note`, `session new|end|list`. Labels before coordinates (a coordinate click
is still logged with the element it hit). Text output by default, `--json`
for tools; screenshots go to a printed file path. `berth mcp` exposes the
same verbs as MCP tools. `docs/agents/SKILL.md` teaches the workflow.

### 4. Guest agents

- Linux: a `gui` module in `berth-agent`: compositors per session as the
  machine's user, capture, input, AT-SPI over D-Bus, window management
  (needs `at-spi2-core`).
- macOS: `berth-gui-mac`, a Swift LaunchAgent on vsock speaking the same
  protocol.

### 5. Activity log and agent identity

JSON lines in `~/.berth/logs/gui/<date>.jsonl` at start and end of every
call: time, target, session, verb, args, result, screenshot, element hit,
and who. Who: Cursor/Codex from their env vars; Claude Code by walking up the
process tree to `claude`; else "you, in a terminal"; MCP passes the client's
identity. Chat titles from the agent's own transcripts, opt-in and local.

### 6. Isolation and concurrency

Linux: `berth gui session new dev` = a fresh headless compositor (cheap; ten
per machine is fine); `--clone` for an APFS copy of the machine. macOS: a
session claims a VM slot; `--clone` starts a throwaway clone of the base;
past 2 running VMs sessions queue. Sessions expire after an idle timeout.

### 7. Berth.app

Display windows (Metal layer for Linux sessions, `VZVirtualMachineView` for
macOS VMs); a macOS VMs section; an Agents section (live thumbnails,
timeline per chat, inspector, per-app timeline; watch-only, "Take over");
menu bar entries for active sessions and the slot queue.

## Phases

Behind `"gui"` / `"mac"` flags in `~/.berth/config.json` until stable.

- **Phase 0, spikes** (`bench/spikes/gpu/`, results in `docs/LEARNINGS.md`):
  S1 custom virtio-gpu binds `virtio_gpu`; S2 virgl through ANGLE; S3
  headless dmabuf export; S4 shared memory region as MTLBuffer; S5 macOS
  provisioning and TCC grants survive cloning; S6 config-space writes.
- **Phase 1**, macOS VMs. Done when `berth mac create test && berth mac reset
  test` works and a clone boots to a logged-in desktop in under 20 s.
- **Phase 2**, agent testing on macOS. Done when an agent using only the
  skill and MCP pushes a SwiftUI app, opens it, clicks a button by label and
  verifies a screenshot; two agents in parallel get separate clones.
- **Phase 3**, Linux display foundation. Done when `weston-simple-egl` and
  `glmark2` run GPU-accelerated and show in a Mac window at display rate.
- **Phase 4**, Linux GUI sessions and desktops. Done when Omarchy is usable
  as a desktop and `berth machine run dev -- firefox` is one Mac window.
- **Phase 5**, agent testing on Linux (GTK and Electron apps; ten parallel
  sessions; live peek).
- **Phase 6**, polish: per-app windows, `berth gui record`, docs.

Phases 1–2 and 3–4 can run in parallel.

## Games readiness

| Games will need | Foundation choice that allows it |
|---|---|
| Vulkan via Venus | standard virtio-gpu with blob resources, context init and a shared memory region from Phase 3 |
| Fullscreen, high refresh, low latency | zero-copy, display-link-paced presentation |
| Mouse capture, relative motion | input channel carries relative motion and pointer lock from Phase 4 |
| Gamepads | versioned input channel; or USB passthrough |
| Low-latency audio | virtio-sound + PipeWire |
| x86-64 games, Steam | Rosetta today; keep binfmt pluggable for FEX |
| Big VRAM-like allocations | shared memory region sized by `gpu_memory_mib` |

## Risks

Custom virtio limits (no config-write callbacks, unknown queue/region
limits); Venus over `MapMemory` unproven; headless capture adds ~a frame;
Hyprland churn (pin a version); no VZ frame-buffer API for macOS host-side
screenshots; TCC grants may not survive clones; licenses (virglrenderer MIT,
ANGLE BSD, Mesa MIT; macOS EULA 2 VMs); transcript reading stays local and
opt-in.

## References

- VM Peek and `testvm`: https://flaviocopes.com/apps/vm-peek/
- RiftVM: https://medium.com/@everettjf/riftvm-running-3d-accelerated-omarchy-on-a-mac-with-macos-27s-custom-virtio-3f845e1dd08e
- WWDC26, Expand the capabilities of your Virtualization app: https://developer.apple.com/videos/play/wwdc2026/224/
- Custom Virtio API via the Go bindings: https://pkg.go.dev/github.com/Code-Hex/vz/v4
- macOS 27 virtualization changes: https://next.ink/246929/macos-27-donne-un-coup-de-fouet-a-sa-virtualisation
- KosmicKrisp: https://www.phoronix.com/news/KosmicKrisp-Vulkan-1.4-2026
- Venus on krunkit: https://lima-vm.io/docs/config/vmtype/krunkit/
