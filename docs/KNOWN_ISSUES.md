# Known issues

Open findings from the code reviews of 2026-10-24 and 2026-10-25 (each
entry re-checked against the code on 2026-10-25), most important first
within each area. Fixed findings aren't listed (see the git log). When you
fix one, remove it here in the same change.

## File sharing (berth's share, `src/fuse.rs`)

- **Past 255 disks, node IDs can collide.** A node ID is the inode number
  with its disk's index folded into the top byte (`node_id`), so the
  `/Volumes` share's disks don't share IDs; from the 256th disk on, they
  share the last index. An inode number using the top byte (not seen on
  APFS) could also collide.
- **A removed file may lose its data.** A file removed while open in the
  guest stays readable only if berth already had it open (its cached file),
  which is usual but not guaranteed (eviction past 4096 cached files is
  skipped only for removed nodes). Reopening by name is refused rather than
  getting the wrong file.
- **`/tmp` and `/var/folders` bind sources are mapped only in container
  creates** (Docker's API and Podman's libpod one), and only in bodies that
  aren't chunked and are under 8 MiB (`src/dockerapi.rs`); other creates get
  the VM's own `/tmp`. Machines' `--mount`, `push` and `pull` resolve them.
  Integrated machines see only home; isolated ones only their `--mount`
  folders.
- **The extra shares are read-write**: the VM can write to `/Volumes`
  (external and network disks), `/private/tmp` and `/private/var/folders`
  (`$TMPDIR`, caches), as it can to home.
- **Missing operations.** RENAME2 with `RENAME_NOREPLACE` returns EINVAL
  (macOS `renamex_np(RENAME_EXCL)` could do it); MKNOD is ENOSYS, so `bind()`
  of a Unix socket in a shared folder fails. No xattrs, no locks beyond what
  the guest kernel does locally.
- **Seen once: `rm -rf` of a just-extracted tree failed** with "Directory
  not empty" (one of ~25 `bench/fs/in-container.sh` runs, 2026-10-25),
  right after a restart; not reproduced since. A listing missing names
  points at the cached directory listings.
- **Stale eviction queue.** Entries for forgotten nodes stay queued, so
  eviction can start early; and files removed while cached are dropped from
  the queue but keep their descriptor until forgotten, outside the 4096 cap.
- **On the Debian kernel, the polling driver is built at runtime** (needs
  the network and ~200 MB of headers and gcc; slower share until then).
  berth's own kernel has it built in (`guest/kernel/README.md`). The very
  first start of a new VM is always Debian's (berth's kernel needs the root
  partition's command line, exported by that boot), so it's on the slower
  driver until berth restarts. `berth status` and the app say when the
  share is on the slower driver, and why.
- **Safe mode needs direct boot.** If the guest kernel was never exported
  (EFI boot only), a broken mount setup can't be rescued automatically.
- **A snapshot restore takes ~16 s**, mostly stopping the current VM. A
  restore could reuse the running VM's process instead.
- **Polling driver: device hot-unplug** (both the 6.12 module and the 7.2
  patch; Virtualization.framework doesn't hot-unplug, so unreachable today). If the device disappeared while a
  task was polling, the poller could touch a freed queue (unmount and `rmmod`
  can't trigger it). Fix: count pollers and wait for them in
  `virtio_fs_drain_queue`.
- **Polling driver: completions in the sender's context** (both forms). Completions of
  other requests can now run in the sending task, which may hold
  `i_rwsem`/folio locks. No callback is known to take those.

## Guest kernel (`guest/kernel`)

- **7.2 isn't a long-term release.** Its fixes come from 7.2.y stable
  releases, which reach users only when berth is rebuilt and released, and
  berth must move to the next series (porting the polling driver) before
  7.2's support ends.
- **6.18 locked up under k3s** (a CPU stuck with interrupts off while k3s
  tore down pod veths, twice, no backtrace). Not understood: it may be
  6.18's, or the polling driver's 6.18 port. Check before ever using 6.18.
- **Two failed starts set berth's kernel aside** until a different kernel
  ships (known by its release and its Image's size and time). A stop within
  20 s of starting isn't counted, but one later while boot hangs is, and a
  safe boot doesn't reset the count. Delete `~/.berth/guest/boot/berth-failed`
  to try it again.
- **Memory a workload used stays with the VM process** until the VM
  restarts. Virtualization.framework's balloon doesn't take back pages the
  guest touched, and it has no free page reporting (what OrbStack's own
  hypervisor uses); the custom Virtio device API can't discard guest
  memory either. The balloon only limits how much more the guest touches.
  Making freed pages cheap for macOS to compress (zeroing them on free)
  would need a kernel change: `init_on_free` also zeroes all memory at boot
  and every page the balloon gives back.
- **Containers can swap** (zram, up to their memory limit, as Docker allows
  when the host has swap): a container over its limit slows down before
  it's OOM-killed. Kernel and socket memory isn't counted against container
  limits (`cgroup.memory=nokmem,nosocket`).

## Guest agent (`agent/`)

- **A container's own domain label can claim any `.local` name**, such as
  `nas.local` (an image's label only names under `berth.local`, and no label
  takes a name berth generated for another container). That's the user's
  choice in a compose file or `--label`; it overrides the real device's name
  on the Mac while the container runs.
- **Network-isolated machines get no IPv6** (it isn't forwarded for them),
  and their rules are iptables ones keyed on the machine's interface: a
  rename re-applies them, a machine renamed by hand outside berth loses them.
- **Intel (amd64) machines** run with systemd's per-service syscall filters
  and MemoryDenyWriteExecute lifted (a drop-in in the machine): x86-64
  filters don't fit Rosetta, which runs on arm64 syscalls and writes code.
- **The machines' sshd is installed on first `ssh <name>.berth`** with the
  distro's package manager: it needs the internet, and the first connection
  waits for it.
- **Integrated machines are trusted with the Mac**: any user in one can run
  commands on the Mac with `mac` (`/run/berth/mac.sock` is 0666), and berth
  enters them with nsenter, running the machine's own binaries as the VM's
  root. For untrusted code use an isolated machine: no `mac`, home or SSH
  agent, and berth reaches into it only through its own systemd, so its
  limits hold (which is why isolated machines need a systemd distribution).
- **`mac` doesn't pass window resizes** to the command on the Mac (it needs a resize message in
  the `mac` protocol, which only sends the size at the start).
- **mDNS:** the goodbye socket briefly shares port 5353; names cached under
  a wildcard get no goodbye.

## Networking

- **Host networking forwards TCP servers bound to the wildcard addresses,
  127.0.0.1 or ::1, and UDP ones bound to the wildcard addresses only**, always
  on the Mac's loopback. A host-network server bound to one specific guest
  address isn't forwarded; UDP on loopback or on an ephemeral-range port
  isn't either. New servers appear on the Mac within the poll: 2 s while a
  host-network container runs, 10 s otherwise (so the first one can take up
  to 10 s).
- **Host-network containers can't reach Mac servers on `localhost`** (they
  can through `host.docker.internal`); see LEARNINGS.
- **IPv6 isn't routed from the Mac to containers.** Container IPv6 addresses
  are reachable from the VM and other containers, not from the Mac (the root
  helper routes IPv4 subnets only). Published ports work over IPv6 as before.
- **Published ports are on the LAN by default** (Docker's semantics); set
  `"ports": "localhost"` to keep them on the Mac.
- **No IPv6 for names:** berth's DNS and mDNS answer A records only
  (`host.docker.internal`, `*.berth.local`), and there is no IPv6
  `host.docker.internal`.
- **Toggling IPv6 restarts dockerd**, which stops running containers (also
  ones kept across a suspend). Turning it off leaves IPv6 forwarding on in the
  VM until it reboots.
- **IPv6 egress is unverified:** the test Mac has no IPv6 internet, so NAT66
  from the VM to the internet hasn't been seen working.

## Host daemon (`src/`)

- **A direct boot that fails costs one more failed start** before EFI takes
  over: an unfinished attempt can also be a stop during boot, so it takes
  two in a row (berth's kernel: see Guest kernel).

- **`~/Berth` (SMB) gives root in the VM** to whoever has the share's
  password (`~/.berth/files-password`, readable only by the Mac user). Links
  are followed only within the share, so a container's absolute symlinks
  show as missing in `~/Berth/containers`.

## Root helper and downloads

- **`/usr/local/bin` links point into `~/.berth/bin`, which the user can
  write.** Root and other accounts on the Mac then run binaries that user
  controls. Fine on a single-user Mac; consider root-owned copies.
- **Downloads:** the VM's base image and machine images are checked against
  checksum files from the same origin (the CLIs and podman-remote have
  pinned SHA-256s; the kernel source's checksums are signed by kernel.org's
  pinned autosigner key).

## App (`app/`)

- **Terminal sessions** are kept for the 8 terminals shown most recently;
  older ones are closed.
- **UI:**
  - logs re-attach when a container's running state changes, which a very
    quick restart can slip between;
  - the Kubernetes screen shows pods, not services;
  - the CLI's `machine push/pull`, `config unset`, `delete`, the `kernel`
    setting and import's `--images/--volumes/--containers` aren't in the
    app.
