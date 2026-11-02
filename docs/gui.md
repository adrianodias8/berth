# Linux GUI machines (experimental)

berth's Linux machines (`berth machine`) can run graphical apps: each GUI
session is its own headless Wayland compositor inside the machine (sway, or
cage for a single app), with accessibility (AT-SPI) on so `berth gui` can
find buttons by label. It's off until you turn it on:

```sh
berth config set gui true
```

The flags below are hidden from `--help` while the feature is experimental.

## Machines with a GUI

```sh
berth machine create dev --distro debian:trixie --gui   # Debian/Ubuntu, Fedora or Arch
berth machine create desk --distro archlinux --gui=omarchy   # Arch only
```

`--gui` installs sway, cage, grim, wtype, D-Bus, at-spi2 and the GTK/Qt
accessibility bridges with the distribution's package manager, plus Mesa,
seatd and a few GL clients to try the GPU with (`eglinfo`, weston's demos,
glmark2 where the distribution has them; a package it doesn't have is
skipped with a warning). `--gui=omarchy` (Arch only, best effort) also
installs Hyprland and Omarchy's desktop packages and clones Omarchy
v4.0.6's configuration; its log is `/var/log/berth-omarchy.log` in the
machine. `berth-agent gui-install <machine> [omarchy]` (as root in the VM)
does the same for a machine that exists.

A GUI machine gets the VM's GPU nodes bound in: the render node
(`/dev/dri/renderD128`) and the display (`/dev/dri/card0`), with a unit
drop-in that allows them. A machine made before a node existed (the render
node comes with virgl) gets it when the agent starts, and restarts once if
it was running; a node that's gone is unbound the same way, since nspawn
won't start a machine whose bind source is missing.

## Running apps

```sh
berth machine run dev -- gnome-calculator     # an app session (cage); prints its id
berth machine display dev                     # list the machine's sessions
berth machine display dev --session s-1a2b --screenshot calc.png
```

Agents use `berth gui` against `linux:<machine>` instead (`docs/agents.md`);
each `berth gui session new` gets its own screen, so two agents never click
in each other's apps.

## Session kinds

A session is one of three kinds (`session.new`'s `"kind"`; by default
`app` when it names an `"app"`, else `agent`):

| kind | compositor | screen |
|---|---|---|
| `agent` | headless sway, any number of apps | its own, 1280×800 by default (`"size"`) |
| `app` | headless cage, one app full screen | its own |
| `desktop` | sway or Hyprland on the VM's display, through seatd | the VM's display (scanout 0 on the Mac) |

A desktop's `"flavour"` is `hyprland` or `sway`. Without one, it's Hyprland
when the machine has Hyprland, hyprctl, seatd and a `~/.config/hypr`
(Omarchy's, on a `--gui=omarchy` machine), else sway. Hyprland starts
through Omarchy's session manager, uwsm, only when Omarchy is installed and
the user's own systemd manager runs (uwsm runs the compositor as one of its
units); otherwise directly. `session.new` and `session.list` say each
session's `kind` and `renderer`, and a desktop's `flavour` and
`"scanout": 0`.

Rendering is GLES on the render node (`"renderer": "gles2"`) when the VM's
GPU does 3D (virgl), else the CPU (`pixman`); a compositor that doesn't
come up on GLES is started again on pixman, its first log kept as
`compositor-gles2.log` in the session's folder. Hyprland has no pixman:
without 3D it uses Mesa's software GL (`software`). Berth.app's Display
window shows a session live when the daemon can stream it, else as
screenshots refreshed every 2 s. The virtio-gpu device is behind the same
`gui` setting (`gpu_memory_mib` and `gpu_display` in
`docs/configuration.md`); its spikes are in `bench/spikes/gpu/`.

## Limits

- Electron/Chromium apps need `--force-renderer-accessibility` for their
  tree (berth passes it when it opens them); Xwayland apps and AppImages
  haven't been tested.
- GTK 4.18 reports some nested widgets' frames about 12 points off; clicks
  aim at the centre, so they still land.
- Apps without accessibility support (foot, for example) have an empty
  tree: use coordinates and screenshots.
- One desktop per VM, across machines: it's the DRM master of card0. A
  second one is refused with the session that has it. The machine's seatd
  (started once, as root, not tied to a VT) stays after the desktop ends.
- A desktop's input comes only from virtual devices (berth's pointer and
  keyboard); the machine has no libinput devices.
- Not yet tried on a live VM: DRM master inside nspawn through seatd,
  Hyprland with no input devices, and its support for the virtual pointer,
  virtual keyboard and screencopy protocols that `click`, `type` and
  `screenshot` use.
