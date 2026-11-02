# macOS VMs (experimental)

`berth mac` runs macOS guests for testing Mac apps in a clean, disposable
copy of macOS instead of on your own desktop. It's off until you turn it on:

```sh
berth config set mac true
```

The commands are hidden from `berth --help` while the feature is
experimental. Berth.app shows a **macOS VMs** section once `mac` is on.

## Create one

```sh
berth mac create test                 # the newest macOS this Mac supports
berth mac create test --ipsw ~/Downloads/UniversalMac_27.2_26B123_Restore.ipsw
berth mac create test --dry-run       # say what would be downloaded and stop
```

`create` downloads the restore image (about 15 GB, kept in
`~/.berth/mac/cache/` and resumed if interrupted), installs it, and boots it
once to set up an account, so Setup Assistant never appears. The account is
`berth` / `berth` unless you pass `--user` and `--password` (a password on
the command line is visible in `ps`). Remote Login is on, and the account
logs in automatically.

Options: `--cpus` (default 4), `--memory-mib` (8192), `--disk-gib` (80; the
image only takes the space the guest writes), `--screen WxH@2x` (default
`1920x1200@2x`, which the guest keeps fixed so screenshots and coordinates
stay stable), `--display` to watch the first boot.

## Use it

```sh
berth mac list                 # name, state, CPUs, memory, disk, base, IP
berth mac start test           # in the background, no window
berth mac start test --display # with a window (also shows a running VM's window)
berth mac shell test           # SSH in (or: berth mac shell test -- sw_vers)
berth mac info test
berth mac stop test            # guest shutdown; stopped hard after a minute
berth mac stop test --force
berth mac delete test
```

Each running VM is its own process (`berth mac run`, hidden), with its
files in `~/.berth/mac/<name>/`: the disk, `run.log`, and a `push/` folder
the guest sees at `/Volumes/My Shared Files/berth-push`.

**At most 2 macOS VMs run at once.** That's Apple's license for macOS
guests; a third `start` is refused and names the running ones. `berth gui`
sessions that need a VM wait in a queue for a free slot.

## Clean state: set-base, clone, reset

Get one VM the way you want it, then save that as the clean state:

```sh
berth mac create base
# set it up: install berth-gui-mac and grant its permissions (below)
berth mac stop base
berth mac set-base base         # saves base/disk.img and base/aux.img
berth mac clone base try-1      # an APFS copy-on-write copy, own identity
berth mac reset try-1           # back to the base's clean state in about a second
```

A clone takes no space until it changes. It gets its own machine identifier
and MAC address, so a base and its clone can run at the same time. `reset`
keeps the clone's identity, stops it hard first if it was running, and
starts it again afterwards. `set-base` also records the VM as the default
base, which `berth gui session new --clone` uses when no target is given.

## The GUI agent and its permissions

`berth gui` talks to `berth-gui-mac` in the guest (`guest/mac-agent`, see
its README). Install it **in the base image**, so every clone has it:

```sh
cd guest/mac-agent && swift build -c release
cp .build/release/berth-gui-mac install.sh dev.berth.gui-mac.plist ~/.berth/mac/base/push/
berth mac start base --display
# in the guest, as the berth user, from /Volumes/My Shared Files/berth-push:
./install.sh ./berth-gui-mac
```

Then, in the VM's window, switch berth-gui-mac on in System Settings ›
Privacy & Security under **Accessibility** and **Screen & System Audio
Recording** (`install.sh` opens both panes), run `./install.sh` again, and
check with `./install.sh --status`. macOS can't grant these from a script,
so this is done by hand once per base image (and again after installing a
new build of the agent). Then stop the VM and run `set-base`.

## Limits

- Apple Silicon only; the guest's macOS must be one this Mac supports.
- A clone's first boot and whether permissions survive cloning haven't been
  measured on every macOS release yet (plan spike S5).
- Socket paths must stay under 104 bytes: very long VM names under a long
  home folder fail to start (see the VM's `run.log`).
