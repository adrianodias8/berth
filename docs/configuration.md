# Configuration

berth reads `~/.berth/config.json` when the VM starts. The app's Settings
and `berth config` write the same file, so you rarely need to edit it:

```sh
berth config list               # every setting, defaults for those not set
berth config set memory_mib 8192
berth config get stop
berth config unset cpus         # back to the default
```

`berth config set` checks the value (`ipv6` and `finder` also take
on/off/yes/no). `berth config` keeps keys it doesn't know; the app writes only
the settings you change. A missing file or key
means the default. Changes apply on the next start (`berth restart`, or Restart in the app),
except `ports`, which applies to new forwards right away.

```json
{
  "cpus": 8,
  "memory_mib": 12288,
  "file_sharing": "berth",
  "ipv6": false,
  "stop": "shutdown",
  "finder": true,
  "proxy": "auto"
}
```

| Key | Default | |
|---|---|---|
| `cpus` | every core | CPUs the VM may use, clamped to what Virtualization.framework allows |
| `memory_mib` | half the Mac's RAM, 2–16 GiB | the most memory the VM may hold. The balloon limits the guest to what it uses plus headroom, so this is a ceiling, not a reservation; memory the guest has touched stays with the VM until it restarts |
| `file_sharing` | `"berth"` on macOS 27 | `"apple"` uses Apple's virtiofs for the home folder, /Volumes, /tmp and $TMPDIR (`docs/file-sharing.md`). Before macOS 27 it's always Apple's |
| `ipv6` | `false` | IPv6 subnets for Docker networks, NAT'd out of the VM. Changing it restarts dockerd, which stops running containers; a suspended VM boots instead of resuming |
| `stop` | `"shutdown"` | `"suspend"`: `berth stop` saves the VM's state and the next start resumes it (see below) |
| `proxy` | `"auto"` | the HTTP(S) proxy Docker uses for pulls and pushes (and apt in the VM): `"auto"` follows the Mac's settings (System Settings › Network › Proxies, checked every 15 s), `"none"`, or an `http://` or `https://` URL. A proxy on the Mac's own `localhost` works. PAC files aren't followed. Containers don't get it: pass `HTTP_PROXY` yourself |
| `kernel` | `"berth"` | `"debian"`: boot the VM's own Debian kernel instead of berth's tuned one (`guest/kernel/README.md`); applies on the next start |
| `ports` | `"all"` | `"localhost"`: published ports listen only on the Mac's loopback (`-p 8080:80` stays off the LAN); applies to new forwards right away |
| `image_store` | `"classic"` | where Docker keeps images. `"classic"` is Docker's overlay2 store (as OrbStack uses); `"containerd"` is containerd's image store, Docker 29's own default, which holds multi-platform images and build attestations but makes each `docker build` ~1.2 s slower (it compresses and unpacks every layer). Each store lists only its own images: after a switch, pull or build them again (the other store's stay on disk until switched back). Changing it restarts dockerd at the next start |
| `finder` | `true` | Docker volumes, machines' files and running containers' root filesystems in Finder at `~/Berth`, over SMB from the VM, while berth runs |

### Experimental

These turn on features that are still being built (`docs/plans/gui-vms-agent-testing.md`).
With both flags off, nothing below runs or writes files.

| Key | Default | |
|---|---|---|
| `mac` | `false` | macOS VMs (`berth mac`, `docs/mac-vms.md`), `mac:*` targets for `berth gui`, and the app's macOS VMs and Agents sections; applies right away |
| `gui` | `false` | Linux GUI machines (`berth machine create --gui`, `docs/gui.md`), `linux:*` targets for `berth gui`, the app's Agents section and Display windows, and (on macOS 27, at the next start) a virtio-gpu display device in the VM |
| `gpu_memory_mib` | `256` | MiB of host memory shared with the VM's GPU device, 16–16384, rounded up to a power of two; with `gui` on, applies on the next start |
| `gpu_display` | `"1920x1080"` | the GPU device's display size, `WIDTHxHEIGHT` from 640x480 to 7680x4320; with `gui` on, applies on the next start |
| `gpu_virgl` | `true` | 3D for the GPU device: virglrenderer on ANGLE, which draws with Metal on the Mac's GPU. Only in a berth built with virgl (`berth gpu-probe` says whether it is, see [3D](#3d)); `false`, or a renderer that doesn't start, leaves the device 2D. With `gui` on, applies on the next start |
| `gpu_blob` | `false` | also offer the guest blob resources and context types (`RESOURCE_BLOB`, `CONTEXT_INIT`), the hooks for Venus/Vulkan; off by default because Linux then allocates its dumb framebuffers as blobs too. Needs `gpu_virgl`; applies on the next start |
| `gui_idle_minutes` | `30` | `berth gui` sessions unused this long are ended by the next `berth gui` call (a throwaway macOS clone is deleted); `0` never ends them |
| `gui_titles` | `false` | name Claude Code chats in the agent activity log by their titles, read from `~/.claude/projects` on this Mac (`docs/agents.md`) |

## Suspend instead of shutdown

With `"stop": "suspend"` (or `berth stop --suspend`), `berth stop` pauses
the VM and saves its memory to `~/.berth/data/suspended/`. The next `berth
start` resumes it in about 3 s, with containers still running.

- macOS saves a VM's state only while the Mac is unlocked. If it can't, the
  VM shuts down instead.
- If the CPUs, memory or file sharing setting changed since, the next start
  boots the saved disk instead of resuming, so the settings apply. That's
  like booting after a power cut: containers are restarted, not resumed.
- A state saved on another macOS build may not resume. berth then boots the
  disk the same way.
- `berth stop --shutdown` always shuts down, and the app's Restart uses it.
- The saved state is as large as the memory the VM was using.

## Proxy changes

Docker reads its proxy when it starts, and restarting it would stop
containers, builds and pulls (with `"auto"` the proxy changes with the Mac's
network, a VPN say), so a change applies when Docker next starts:
`berth restart`. Just after the VM boots, before
anything runs, berth does restart Docker once if the proxy changed while
the VM was off.

## Environment variables

These are for testing and debugging; set them for `berth start`.

| Variable | |
|---|---|
| `BERTH_FILE_SHARING` | `apple` or `berth`; overrides `file_sharing` |
| `BERTH_FS_POLL_DRIVER` | `1`/`0`: force the polling guest driver on or off (normally automatic) |
| `BERTH_FS_ENTRY_TIMEOUT`, `BERTH_FS_ATTR_TIMEOUT`, `BERTH_FS_NEGATIVE_TIMEOUT` | seconds the guest may cache names, attributes and missing names (default 1) |
| `BERTH_FS_POLL_US` | how long the host polls idle queues, in µs (default 100) |
| `BERTH_FS_THREADS`, `BERTH_FS_QUEUES`, `BERTH_FS_WRITEBACK`, `BERTH_FS_NO_OPENDIR`, `BERTH_FS_ASYNC_READ` | file server tuning, see `docs/file-sharing.md` |
| `BERTH_FS_STATS`, `BERTH_FS_TRACE` | `1`: per-request timings, or every request, in `daemon.log` |
| `BERTH_GPU_VIRGL` | `0`: the GPU device stays 2D whatever `gpu_virgl` says |

## 3D

A berth built with the `virgl` cargo feature links virglrenderer and
ANGLE's libEGL, and gives the Linux display 3D (`gpu_virgl`). The libraries
come from `bench/spikes/gpu/s2-virgl-angle.sh` (spike S2), which builds
them into `target/spikes/gpu/prefix`. These are read when building, not by
`berth start`:

| Variable | |
|---|---|
| `BERTH_VIRGL` | `auto` (default): `scripts/build.sh` builds with virgl when the prefix has virglrenderer; `1` requires it (and fails without the libraries); `0` builds without. The script prints `berth: virgl on` or `off` |
| `BERTH_VIRGL_PREFIX` | where the libraries are (`lib/`), for `scripts/build.sh` and for `cargo build --features virgl`; default `target/spikes/gpu/prefix` |

A development build finds the libraries in the prefix through its rpath,
so `cargo test --features virgl` needs no `RUSTFLAGS` or `DYLD_*`.
`scripts/build-app.sh` copies them into `Berth.app/Contents/Frameworks`
(virglrenderer, libepoxy, libEGL and libGLESv2), drops the rpath to the
prefix and ad-hoc signs them, and `scripts/check-bundle.sh` checks the
result. The app isn't signed with the hardened runtime: under it, the
libraries would need the same Developer ID as the app (`scripts/release.sh`
does that when `BERTH_SIGN_IDENTITY` is set).

`berth gpu-probe` (hidden) starts the renderer in its own process, with
no daemon, and prints the capsets it would offer (`capset 2 VIRGL2 v2
…`) and ANGLE's GL renderer; `--json` for a script. It exits 0 when the
renderer starts, 1 when it doesn't, and 2 when berth was built without
virgl.

## Files

| Path | |
|---|---|
| `~/.berth/config.json` | this file |
| `~/.berth/data/` | the VM: disk image, EFI variables, machine identity, a suspended state |
| `~/.berth/snapshots/` | snapshots (excluded from Time Machine) |
| `~/.berth/guest/` | the guest agent, the exported Debian kernel, berth's kernel's module data (`kernel-modules.tar`), boot markers, and `ca-certificates.pem`: the CA certificates the Mac trusts beyond Apple's roots, which the VM adds to its trust store before Docker starts (containers don't get them) |
| `~/.berth/bin/` | the bundled `docker`, Compose, Buildx and `kubectl` |
| `~/.berth/files-password` | the password of the VM's SMB share for `~/Berth` (only you can read it) |
| `~/.berth/cache/` | the downloaded Debian image (kept by `berth delete`; excluded from Time Machine) |
| `~/.berth/run/` | sockets (`docker.sock`, `podman.sock`, `ssh.sock`, `machine-ssh.sock`, the control socket) and the daemon's pid file |
| `~/.berth/tls/` | the local CA for `https://*.berth.local` |
| `~/.berth/log/` | `daemon.log` (the host side) and `console.log` (the VM's console) |
| `~/.berth/ssh/` | the VM's SSH key, and `config` (`ssh berth`, `ssh <machine>.berth`), included from `~/.ssh/config` |
