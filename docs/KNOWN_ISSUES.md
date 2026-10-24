# Known issues

Open findings from the code review of 2026-10-24, most important first
within each area. Fixed findings aren't listed (see the git log). When you
fix one, remove it here in the same change.

## File sharing (berth's share, `src/fuse.rs`)

- **A removed file may lose its data.** A file removed while open in the
  guest stays readable only if berth already had it open (its cached file),
  which is usual but not guaranteed (eviction past 4096 cached files is
  skipped only for removed nodes). Reopening by name is refused rather than
  getting the wrong file.
- **`/tmp` and `/var/folders` bind sources are mapped only in container
  creates** (Docker's API and Podman's libpod one): machines don't get the
  mapping (use `/private/tmp/...`). Integrated machines see only home;
  isolated ones only their `--mount` folders.
- **The extra shares are read-write**: the VM can write to `/Volumes`
  (external and network disks), `/private/tmp` and `/private/var/folders`
  (`$TMPDIR`, caches), as it can to home.
- **Missing operations.** RENAME2 with `RENAME_NOREPLACE` returns EINVAL
  (macOS `renamex_np(RENAME_EXCL)` could do it); MKNOD is ENOSYS, so `bind()`
  of a Unix socket in a shared folder fails. No xattrs, no locks beyond what
  the guest kernel does locally.
- **Stale eviction queue.** Entries for forgotten nodes stay queued, so
  eviction can start early.
- **Module build needs network and ~200 MB.** The agent fetches kernel
  headers and gcc with apt. Offline, or on a kernel Debian no longer ships
  headers for, the share runs on the built-in driver (slower). Shipping
  prebuilt modules per Debian kernel would avoid both.
- **Safe mode needs direct boot.** If the guest kernel was never exported
  (EFI boot only), a broken mount setup can't be rescued automatically.
- **A snapshot restore takes ~16 s**, mostly stopping the current VM. A
  restore could reuse the running VM's process instead.
- **Polling driver: device hot-unplug.** If the device disappeared while a
  task was polling, the poller could touch a freed queue (unmount and `rmmod`
  can't trigger it). Fix: count pollers and wait for them in
  `virtio_fs_drain_queue`.
- **Polling driver: completions in the sender's context.** Completions of
  other requests can now run in the sending task, which may hold
  `i_rwsem`/folio locks. No callback is known to take those.

## Guest agent (`agent/`)

- **No limits on connections.** Each connection to the web (`web.rs`) and
  `host.docker.internal` (`hostnet.rs`) proxies gets its own thread, with no
  limit.
- **DNAT rules are flushed and rebuilt on each sync.** That leaves a short
  window with no DNAT, during which a service IP reaches the guest itself.
  Use `iptables-restore --noflush`, or diff against the existing rules.
- **Label domains can claim any `.local` name**, such as `nas.local` (they
  can't take a name berth generated for another container). Consider an
  opt-in for names outside `berth.local`.
- **Network-isolated machines get no IPv6** (it isn't forwarded for them),
  and their rules are iptables ones keyed on the machine's interface: a
  rename re-applies them, a machine renamed by hand outside berth loses them.
- **Intel (amd64) machines** run with systemd's per-service syscall filters
  and MemoryDenyWriteExecute lifted (a drop-in in the machine): x86-64
  filters don't fit Rosetta, which runs on arm64 syscalls and writes code.
- **The machines' sshd is installed on first `ssh <name>.berth`** with the
  distro's package manager: it needs the internet, and the first connection
  waits for it.
- **Machine creation puts the user name and home into `sh -c` and `.nspawn`
  files unquoted.** These values come only from the Mac user.
- **Integrated machines are trusted with the Mac**: any user in one can run
  commands on the Mac with `mac` (`/run/berth/mac.sock` is 0666), and berth
  enters them with nsenter, running the machine's own binaries as the VM's
  root. For untrusted code use an isolated machine: no `mac`, home or SSH
  agent, and berth reaches into it only through its own systemd, so its
  limits hold (which is why isolated machines need a systemd distribution).
- **`mac` doesn't pass window resizes** to the command on the Mac.
- **mDNS:** no cap on the number of questions per query; the goodbye socket
  briefly shares port 5353; names cached under a wildcard get no goodbye.

## Networking

- **Host networking forwards TCP servers bound to the wildcard addresses,
  127.0.0.1 or ::1, and UDP ones bound to the wildcard addresses only**, always
  on the Mac's loopback. A host-network server bound to one specific guest
  address isn't forwarded; UDP on loopback or on an ephemeral-range port
  isn't either. New servers appear on the Mac within the 2 s poll.
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

- **Direct boot gets turned off** if berth stops after the "trying" marker
  is written but before the guest clears it.
- **Small edge cases:**
  - an absurd `memory_mib` put in config.json by hand wraps around
    (`berth config set` checks it);
  - a Docker binding only on `::1` maps to 127.0.0.1.

- **`~/Berth` (SMB) gives root in the VM** to whoever has the share's
  password (`~/.berth/files-password`, readable only by the Mac user). Links
  are followed only within the share, so a container's absolute symlinks
  show as missing in `~/Berth/containers`.

## Root helper and downloads

- **Routes leak if the helper is killed.** The helper keeps its route list
  in memory only, so after SIGKILL or a crash the routes stay until berth
  asks for the same subnet again.
- **The route gateway may be any `bridge*` interface,** including
  Thunderbolt Bridge, not only berth's vmnet bridge. Verify against the VM's
  subnet.
- **`/usr/local/bin` links point into `~/.berth/bin`, which the user can
  write.** Root and other accounts on the Mac then run binaries that user
  controls. Fine on a single-user Mac; consider root-owned copies.
- **Downloads:**
  - the Docker CLI tarball is checked only by HTTPS, and extracted in place
    rather than to a temp folder and renamed;
  - other checksums come from the same origin as the files;
  - pin SHA-256s per version, as the podman-remote download already does.

## App (`app/`)

- **Blocking work fills Swift's shared thread pool.** Process runs and socket
  reads happen inside `Task.detached` (start, machine actions, settings,
  stats), and that pool has a thread per core. Run blocking work on threads
  or dispatch queues and resume through continuations, as `stream()` does.
- **Terminal sessions:** the exec session's fields are unsynchronized, and a
  quick tab switch during the exec upgrade can leak a thread and an fd.
  Sessions also end on every tab switch.
- **Follow streams:** a stream cancelled before its socket is set keeps a
  thread blocked.
- **Docker archive endpoint:** file view and download fail on PAX headers
  (long names, xattrs).
- **`TLSTrust.cached` is written off the main actor.**
- **UI:**
  - logs re-attach when a container's running state changes, which a very
    quick restart can slip between;
  - a failed file preview replaces the table;
  - snapshot actions' busy state lives in the view: leaving and coming back
    mid-operation allows a second one;
  - the Kubernetes screen shows pods, not services;
  - the CLI's `machine push/pull`, `config unset`, `delete` and import's
    `--images/--volumes/--containers` aren't in the app.
