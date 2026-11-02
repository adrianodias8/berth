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
`berth` with the password `berth` (or `--user`, `--password`; a password on
the command line is visible in `ps`). It logs in automatically and Remote
Login is on. Once the account exists, `create` adds berth's SSH key to it and
turns off password logins over SSH: the Linux VM's containers share the same
NAT subnet, and a well-known password must not let them in. The password then
only works at the VM's own screen, for System Settings' prompts.

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
berth mac create base           # in a terminal: walks you through the agent's two grants
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
its README), which berth carries inside it. `create` installs it, then, in a
terminal, opens the VM's window and one System Settings pane at a time:
switch berth-gui-mac on under **Accessibility**, press Enter, then under
**Screen & System Audio Recording**, press Enter. The password, if macOS
asks, is `berth`. berth checks each grant and restarts the agent so Screen
Recording applies.

macOS takes these grants only from a person clicking in System Settings, so
this is once per base image. Clones keep them, and so do newer builds of
the agent (it's signed so that they match). When `create` runs without a
terminal, as an agent would run it, it installs berth-gui-mac and says to
run the grants later:

```sh
berth mac grant base            # installs the agent if needed, then the two switches
```

## Limits

- Apple Silicon only; the guest's macOS must be one this Mac supports.
- Clones keeping the grants and booting in ~10 s was measured on macOS 27.2
  (spike S5); other macOS releases haven't been checked.
- Socket paths must stay under 104 bytes: very long VM names under a long
  home folder fail to start (see the VM's `run.log`).
