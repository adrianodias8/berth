# Learnings

What building berth taught us: platform quirks, measurements and dead ends,
so the next person doesn't rediscover them. Newest knowledge wins; when
something here stops being true, fix it.

## Ground rules

- berth is a clean-room project. It's built from OrbStack's public
  documentation and by observing a running OrbStack from the outside (names it
  answers, routes, mounts, timings). Never copy, decompile or translate
  OrbStack code or binaries.
- Measure before optimizing. Several "obvious" wins (threads, more queues,
  write-back caching, bigger poll windows) measured as neutral or worse.

## Virtualization.framework

- **The VM lives on the main dispatch queue.** `berth daemon` calls
  `dispatch_main`, and everything touching `VZVirtualMachine` runs there.
  Keep the VM handle in a `static Mutex`, not a `thread_local`, or other
  queues never see it.
- **Entitlement.** The binary needs `com.apple.security.virtualization`;
  `scripts/sign.sh` signs ad hoc, and is also the cargo runner.
- **Direct boot.** EFI → GRUB costs seconds. The agent exports the guest's
  kernel, initrd and cmdline; later starts use `VZLinuxBootLoader`, falling
  back to EFI if direct boot fails (the "trying"/"direct-failed" markers).
- **The memory balloon doesn't return touched memory to macOS.** Inflating it
  shrinks what the guest may use, but pages it already dirtied stay with the
  VM process. Debian's `init_on_alloc=1` zeroes every page the guest
  allocates and inflates host usage, so berth boots with `init_on_alloc=0`.
  Real reclaiming needs free-page reporting, which VZ doesn't do.
- **The balloon counts as "used" inside the guest** (deflate-on-OOM).
  Subtract the balloon pages from `/proc/vmstat` before computing usage.
- **ASIF disks** (macOS 26+): `diskutil image create … --format ASIF`, then
  `diskutil image resize --image-only 64G`. Space freed in the guest goes
  back to the Mac.
- **macOS 27 custom Virtio devices** (`VZCustomVirtioDeviceConfiguration`):
  - all device and queue calls run on one serial queue;
  - `nextElement` disables notifications until the queue is drained;
  - notification delivery to our code costs ~70 µs, and so does getting the
    reply interrupt back into the guest;
  - there's no API for interrupt control, but shared memory regions exist
    (relevant for a future GPU);
  - the classes aren't in objc2-virtualization 0.3.2: declare them with
    `extern_class!`/`extern_protocol!` and look them up at runtime.

- **Rosetta's translation cache isn't worth it here (measured 2026-10-24).**
  `VZLinuxRosettaUnixSocketCachingOptions` plus `rosettad daemon` in the
  guest works: it binds `<cache>/uds/rosetta.sock` (link
  `/run/rosettad` to it). But containers only reach it through a bind mount
  berth would have to inject into every container, rosettad dies on large
  binaries (Node's: "Not all x86 entry points were contained in a code
  segment"), and the gain is ~20 ms per process: `php -v` 0.15 -> 0.13 s,
  `composer --version` 0.33 -> 0.32 s, CPU-bound PHP unchanged, while a
  binary's first run takes ~1 s longer.
- **Idle wakeups (2026-10-24):** measure per thread in the guest with
  `/proc/*/task/*/schedstat` (third field) and on the Mac with `top -stats
  idlew` on the VM process. What woke an idle VM was mostly berth itself:
  - the agent following Podman's API event stream kept its socket-activated
    service running (~60 wakeups/s), so Podman events come from the journal
    and Podman is asked only when a `conmon` runs;
  - the app's status poll pinged Docker every 2 s.
  Guest: 118 -> 56 runs/s; VM + daemon idle CPU 2.97 % -> 1.37 %. The rest is
  mostly Docker's own containerd and dockerd.

- **Kernel choice (2026-10-25):** 7.2.12 over 6.12 (same config): kernel
  boot 0.24 s vs 0.31 s, drush site:install ~10 % faster, idle the same.
  6.18.58 locked up a CPU when k3s tore down pod veths (twice, no backtrace).
  Porting the polling driver across series: 6.17+ gate legacy iptables behind
  `NETFILTER_XTABLES_LEGACY`, and 7.2 drops `req->fm` (use the connection's
  `mounts` list) and checks replies with `virtio_fs_verify_response`.
- **From OrbStack's running kernel** (its `/proc/cmdline` and sysfs; its
  `/proc/config.gz` is a curated extract without core options): 7.2.12 with
  full preemption, zram swap the size of RAM, swappiness 20, THP `madvise`,
  no disk scheduler, `workqueue.power_efficient=1`,
  `cgroup.memory=nokmem,nosocket`. berth does all of these. Not copied:
  `id_aa64mmfr0.ecv=1` (forces a timer feature the hypervisor hides; unknown
  on M1) and `isolcpus=11` (purpose unknown).

## Snapshots

- **What works:** `VZVirtualMachineConfiguration.validateSaveRestoreSupport`
  passes for berth's normal configuration: virtio-fs shares, Rosetta, vsock,
  NAT, balloon and the console. The macOS 27 custom Virtio device doesn't
  support it unless it implements save hooks (`supportsSaveRestore`).
- **The Mac must be unlocked to save state:**
  - **What happens:** `saveMachineStateToURL` encrypts the state with a
    Secure Enclave key created "when unlocked", so with the screen locked it
    fails with "permission denied". The log shows
    `ctkd … unable to generate key … e00002e2`, and the key API reports
    `errSecInteractionNotAllowed`.
  - **What it isn't:** signing identity, an embedded Info.plist or the save
    location (none of them made a difference).
  - **Fallback:** a disk-only snapshot.
- **The save file must not exist yet;** the framework creates it. Restore
  needs the same host, a macOS build no older than the save's, and the same
  hardware configuration (CPU count, memory), so a snapshot keeps its own
  values and restore uses them.
- **Disk clones:** `fsync` the disk image, then `clonefile`, with the VM
  paused (memory snapshots need nothing else: memory and disk come from the
  same instant). For disk-only snapshots, `sync` in the guest first.
- **After resuming a saved state:**
  - the guest's clock is still at save time, so the agent sets it;
  - the direct-boot "trying" marker must be cleared, since nothing in the
    guest will;
  - a disk-only restore boots, and Docker starts only containers with a
    restart policy, so berth starts the ones that were running.

- **Suspend instead of shutdown:** `berth stop` reuses snapshot machinery.
  It saves the state (about 0.5 GB for an idle VM), stops the VM without a
  guest shutdown so the disk stays as saved, and leaves a pending restore
  marked "consumed". Measured: stop 4 s instead of ~12, start 2.7 s with
  containers still running.
- **What a resumed VM needs:**
  - the same devices it was saved with, so snapshots and suspends record
    the home share (Apple's, berth's, or berth's with the polling driver)
    and restores reproduce it;
  - the clock, which is still at save time, set again;
  - an agent restart if berth was updated meanwhile (`info version` against
    the embedded agent's hash, then `exit`; systemd restarts it).

## Networking

- **Container IPs need routes,** and routes need root. OrbStack uses per-network
  vmnet interfaces, which needs an entitlement Apple grants only to approved
  developers, so berth installs a small root LaunchDaemon (`src/helper.rs`).
  ARP-based guest IP discovery doesn't work for a signed binary; ask the
  guest over SSH/vsock instead.
- **launchd quirks:**
  - helpers run with `HOME` unset, so handle `berth helper …` before
    computing paths;
  - `launchctl bootstrap` returns EIO if the old instance is still being torn
    down, so wait and retry;
  - launchd starts processes with a soft limit of 256 file descriptors, so
    the daemon raises its own limit.
- **Docker 28+ drops traffic to container IPs from outside** (raw DROP). Set
  `allow-direct-routing: true` in `daemon.json` (docker-ce only).
- **Docker's FORWARD policy is DROP**, which also kills Podman and nerdctl
  traffic. berth adds `DOCKER-USER` accepts for DNAT'd traffic, the VM's
  NAT subnet, and `podman+`, `nerdctl+` and `ve-+` interfaces.
- **vsock's ceiling is the device's:** Mac -> VM tops out at ~6 Gbit/s in
  total, however many connections, after widening buffers (macOS Unix
  socket 8 KB by default; Linux vsock credit 256 KB). Splitting a transfer
  over connections can't help. berth's own virtio-fs device, carrying
  connections as FUSE stream files (FOPEN_DIRECT_IO | FOPEN_STREAM), does
  21-25 Gbit/s. Apple's NAT network does ~10.5 Gbit/s Mac -> VM.
- **Ports below 1024:** macOS lets ordinary users bind them only on all
  interfaces, not `127.0.0.1`. DDEV's router binds `127.0.0.1:80/443`, so the
  helper binds those and passes the socket back over SCM_RIGHTS.
- **`.local` names resolve over mDNS on every interface,** including the VM's,
  so berth's responder needs no `/etc/resolver` file.
- **Port forwarding speed:** splice(2) in the agent and 256 KiB copy buffers
  took localhost forwarding from 5.8 to ~20 Gbit/s (OrbStack: ~30).
- **UDP forwards:** reply tasks must abort when the forward is dropped, or the
  port stays bound.
- **Host-network containers:**
  - **Telling them apart:** their sockets sit in the guest's root network
    namespace next to sshd, dockerd's port proxies, systemd-resolved, k3s and
    the agent. The owning process's cgroup separates them cleanly
    (`/system.slice/docker-<id>.scope` with the systemd driver; Podman
    `libpod-<id>.scope`, nerdctl `nerdctl-<id>.scope`); a port exclusion list
    would always be out of date. Map socket inodes from `/proc/net/*` to
    processes through `/proc/<pid>/fd` only for new inodes.
  - **UDP:** a client socket that sends without binding shows up unconnected
    on an ephemeral port, like a server would. Ports in
    `ip_local_port_range` are skipped for UDP.
  - **Loopback addresses:** only 127.0.0.1 and ::1 map to the Mac;
    127.0.0.53 (resolved) and specific guest addresses don't.
  - **Mac servers on `localhost` from a host-network container** (OrbStack
    documents this) aren't offered: the container's loopback is the guest's,
    and redirecting connections to ports nobody listens on would need
    rewriting the guest's loopback (iptables can't match "closed port").
    `host.docker.internal` works in host-network containers (their resolver
    is berth's DNS, and the guest's OUTPUT chain redirects 198.19.248.254).
  - **macOS binds:** `[::]:80` and `127.0.0.1:80` can be bound at the same
    time (the specific address wins), so a host-network server and a
    loopback-published port on the same number coexist.
- **IPv6 on Apple's NAT** (`VZNATNetworkDeviceAttachment`):
  - the Mac sends router advertisements for a ULA /64 (`fdxx:…/64` on
    `bridge100`), so the guest always has an IPv6 address and reaches the Mac
    at the Mac's address in that prefix;
  - on a Mac without IPv6 internet the advertised router lifetime is 0: no
    default route, `Network unreachable` for global addresses. With IPv6
    upstream the Mac is expected to route (vmnet does NAT66 for its prefix);
    not verified yet on such a network;
  - systemd-networkd (not the kernel) handles the advertisements, and with
    `IPv6AcceptRA` unset it ignores them once IPv6 forwarding is on, which
    Docker turns on for IPv6 networks. berth adds `IPv6AcceptRA=yes` for the
    uplink when IPv6 is enabled.
- **Docker IPv6** (29): a network created with IPv6 and no subnet gets a /64
  from a ULA pool Docker derives itself (e.g. `fdbd:669f:fb69::/64`), and
  `default-network-opts.bridge."com.docker.network.enable_ipv6"` turns IPv6 on
  for every new bridge network. ip6tables (default on) masquerades each IPv6
  subnet leaving through another interface. Existing networks keep their
  settings until recreated.
- **ULA sources prefer IPv4:** with only a ULA address, glibc's address
  selection (RFC 6724) puts IPv4 first for global destinations, so dual-stack
  names keep using IPv4 in containers.

## Docker, engines and tools

- **Upgrading docker-ce over an old containerd config** needs `--force-confnew`.
- **Podman:** `host.docker.internal` comes from `containers.conf.d`, and
  `podman.service` must restart to read it. Keep the macOS `podman-remote`
  version equal to the server's.
- **nerdctl** has no API socket, but its `inspect` output is
  Docker-compatible: berth maps it into `/containers/json` shape.
- **CLI downloads:** pin versions and verify SHA-256 against the GitHub
  release asset digest.
- **Docker 29 rejects one-character container names.**
- `alpine:latest` can get retagged to amd64 when pulled for another platform.
- `/usr/local/bin` links: create them only where the tool is missing or the
  link dangles. Don't replace another app's working links.

## File sharing (details in docs/file-sharing.md)

- **OrbStack uses plain virtiofs too** (one `mac` share mounted at `/Users`,
  `/Applications`, `/Library`, `/Volumes`, `/private`). Its speed comes from its
  own VMM, not the protocol.
- **Request counts are set by the tool and the guest kernel,** not the server:
  - **tar:** 5 per file (LOOKUP, CREATE, WRITE, SETATTR, GETATTR of the
    folder);
  - **Composer:** ~10-12, including 2–3 `chmod`s per file, usually to the mode
    it already has;
  - **why some can't be cut:** `O_EXCL` creates and `mkdir` always revalidate
    a cached "doesn't exist" entry, and creating a file always discards its
    folder's cached attributes.
- **Per-request cost dominates.** The fixes that mattered:
  - polling the virtqueues on both sides;
  - skipping no-op `chmod`s;
  - working through open file descriptors rather than paths;
  - negative-entry caching;
  - not echoing the guest's own writes back to it.
- **APFS:** creating a file costs ~53 µs however you call it (path lookup is
  ~4.5 µs of that). Concurrent metadata changes don't scale: serving goes from
  ~13 to ~18 µs per request with 4 threads, so threads barely help.
- **FUSE flag traps:**
  - `FUSE_ATOMIC_O_TRUNC` plus no-open mode loses truncation: a rewritten,
    shorter file keeps its old tail. That corrupted `composer.json`.
  - A no-open kernel never sends RELEASE, even for files it created, so
    CREATE must not hand out handles: one leaked descriptor per file
    crashed berth.
  - Write-back caching adds a SETATTR per file and still flushes on close.
- **Coherency:**
  - **ESTALE:** answering ESTALE for an operation on a vanished node makes
    Linux retry the whole lookup (`LOOKUP_REVAL`).
  - **No push invalidation:** Linux's virtio-fs has no notification queue,
    so Mac-side changes reach the guest only through touches (via FSEvents,
    ~0.6 s) or when cache entries expire.
  - **Deleted folders:** of removed paths only the topmost needs a touch:
    the guest then drops its whole cached subtree.
  - **Echo suppression:** compare the exact state (modification time and
    size, or removed) the guest's change left. Time windows broke real
    deletions.
- **`mount(8)` canonicalizes sources:**
  - **The trap:** a source like `home` becomes `/home` when such a directory
    exists. It doesn't happen for types `mount` knows (`virtiofs`), but it
    does for helpers (`mount.berth` got `/home`) and for unknown types
    (`virtiofs_poll`). So the virtio-fs tag didn't match, home didn't mount,
    and the agent, which lives in home, never started.
  - **The fix:** strip the path in the helper and use `--no-canonicalize`.
- **Anything in the boot path needs a way back.** Booting with
  `fstab=0 systemd.mount-extra=home:<path>:virtiofs:rw` (and `rw` instead of
  `ro`) brings a guest up whatever its fstab says; berth's safe mode does
  this automatically.
- **Symlinks:** macOS has `O_NOFOLLOW_ANY`, `AT_SYMLINK_NOFOLLOW_ANY` and
  `RENAME_NOFOLLOW_ANY`, but no flag for `mkdirat`, `symlinkat` or `linkat`;
  open the parent with `O_NOFOLLOW_ANY` for those. Tests: the temp folder is
  under `/var`, itself a symlink.
- **Custom Virtio devices and snapshots:** set `supportsSaveRestore` and
  implement the two delegate methods. Use `method_id` in objc2's
  `define_class!` for the one returning NSData. Let in-flight requests
  finish before saving.
- **Ownership:** guest root extracting tarballs, or Linux group IDs, makes
  `chown` fail on the Mac. Ignore EPERM like Apple's share does.
- **The VM's Debian kernel** (6.12; berth's own is 7.2) builds FUSE and
  virtio-fs in (`=y`), exports the symbols a module needs, has no preempt count
  (`PREEMPT_VOLUNTARY`), and supports multiqueue virtio-fs (6.11+).
- **Dead ends:**
  - an NFS server over the VM network (the nfsserve prototype spun the CPU
    and was slow);
  - more request queues;
  - a dedicated spinning worker pool (no better than GCD);
  - bigger host poll windows.

## The guest and inotify

- **virtiofs carries no macOS file events.** berth watches bind-mount sources
  with FSEvents, and the agent `chmod`s each changed path to its current
  mode. That raises IN_ATTRIB and refreshes cached attributes.
- **Touches raise only IN_ATTRIB**, which chokidar (Vite, nodemon) turns
  into add/unlink by rescanning, but watchpack (webpack, Laravel Mix) and
  @parcel/watcher (Tailwind 4) ignore for deletes. So removals are replayed:
  the agent unlinks the name in the guest and the file server agrees for
  that agent thread only (FUSE requests carry the caller's TID), returning
  the node the guest already had. It never removes anything on a replay: an
  editor may have written the name again meanwhile.
- **Feedback loops:** a touch is a change too. Drop events whose
  (exists, mtime, size) didn't change, and skip the file server's own writes.

## The app (SwiftUI)

- **Lazy statics:** one that evaluates itself recursively (TLS trust check)
  deadlocks or crashes. Use a plain function.
- **Certificate trust:** `security verify-cert` reports a correctly trusted
  local CA as untrusted (Certificate Transparency). Check the keychain's trust
  settings instead (`SecTrustSettingsCopyTrustSettings`; CLI:
  `dump-trust-settings` + SHA-1 fingerprint).
- **Name collision:** macOS volumes are case-insensitive, so `Berth.app`'s
  executable and the `berth` CLI collide in `Contents/MacOS`. The CLI lives in
  `Contents/Helpers`.
- **Layout and formatting:**
  - menu-bar lists collapse without an explicit height;
  - `HSplitView` can overflow, so use fixed-width stacks;
  - format numbers yourself ("8.080" from locale formatting).
- **Setup in the UI:** helper install and CA trust run from the app with an
  admin prompt. Users shouldn't need a terminal.

## Benchmarks (M2 Max, vs OrbStack 2.2.5; see bench/results)

| | berth | OrbStack |
|---|---|---|
| startup | 3.3–3.6 s | 2.6–4.4 s |
| idle memory (system-wide) | +1.37 GB (+1.9 with Podman and nerdctl on) | +1.28 GB |
| idle CPU | 3.0 % | 2.2 % |
| localhost forward | ~20 Gbit/s | ~31 Gbit/s |
| bind-mount extract (20k files) | 7.9 s (Apple's) / 2.4–2.7 s (own share + polling) | 2.3 s |
| DDEV composer install | 9.0–9.2 s (own share + polling) | 6.6 s |

(`bench/results/2026-10-24-compare.md`.)

Neither returns workload memory to macOS within 2 minutes. Docker volumes are
~15x faster than any bind mount (they live on the VM's own disk).

## Testing pitfalls we hit

- **The kernel NFS server can't export overlay mounts** (containers' root
  filesystems), and Debian 13 has no unfs3 (nfs-ganesha needs the same
  export support). Samba reads them like any folder: `~/Berth` is SMB,
  mounted through NetFS with a password.
- **Container root filesystems come and go with the container.** A bind
  mount of one keeps a stopped container's overlay alive, and its restart
  then mounts the same upper directory twice. A recursive bind of Docker's
  mount area made a slave (`mount --make-rslave`) follows its mounts and
  unmounts instead.
- **Running a machine's own binaries through nsenter runs them as the VM's
  unconfined root** (no capability bounding set, seccomp or device rules):
  for an untrusted machine go through its systemd (`systemd-run -M`).
  `machinectl shell` drops a command's output without a terminal.
- **busybox tar creates files with O_CREAT|O_EXCL|O_NOFOLLOW**, and macOS
  rejects O_NOFOLLOW together with O_NOFOLLOW_ANY (EINVAL). A benchmark that
  doesn't check a step's status times its failure.
- **Never `rm` a script while it runs,** or a cleanup path inside your own
  benchmark folder.
- **Timing:** `bash`'s `time` on a subshell with `exec 2>/dev/null` prints
  nothing. Use `time { (…) 2>/dev/null; }`.
- **macOS tar:** it adds `LIBARCHIVE.xattr` headers that GNU tar warns about;
  create benchmark tarballs with
  `COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata`.
- **DDEV:**
  - Drupal CMS ships its own `.ddev/config.yaml`, which replaces yours: put
    overrides in `.ddev/config.local.yaml`;
  - DDEV may wait on stdin, so run it with `</dev/null`;
  - the project name comes from the folder name.
- **Stats:** a FUSE profile needs counts between two quiet points;
  `BERTH_FS_STATS` logs an "(idle)" snapshot after a second of quiet.
