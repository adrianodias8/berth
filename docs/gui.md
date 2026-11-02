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
accessibility bridges with the distribution's package manager.
`--gui=omarchy` (Arch only, best effort) also installs Hyprland and
Omarchy's desktop packages and clones Omarchy v4.0.6's configuration; its
log is `/var/log/berth-omarchy.log` in the machine.

## Running apps

```sh
berth machine run dev -- gnome-calculator     # an app session (cage); prints its id
berth machine display dev                     # list the machine's sessions
berth machine display dev --session s-1a2b --screenshot calc.png
```

Agents use `berth gui` against `linux:<machine>` instead (`docs/agents.md`);
each `berth gui session new` gets its own screen, so two agents never click
in each other's apps.

Rendering is on the CPU (pixman) for now. A live window for a machine's
screen comes with the GPU display: berth has a virtio-gpu device for the VM
(`src/gpu`, behind the same `gui` setting; `gpu_memory_mib` and
`gpu_display` in `docs/configuration.md`), whose spikes are in
`bench/spikes/gpu/`. Until then Berth.app's Display window shows a session
as screenshots, refreshed every 2 s.

## Limits

- Electron/Chromium apps need `--force-renderer-accessibility` for their
  tree (berth passes it when it opens them); Xwayland apps and AppImages
  haven't been tested.
- GTK 4.18 reports some nested widgets' frames about 12 points off; clicks
  aim at the centre, so they still land.
- Apps without accessibility support (foot, for example) have an empty
  tree: use coordinates and screenshots.
