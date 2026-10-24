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
- **The machines' sshd is installed on first `ssh <name>.berth`** with the
  distro's package manager: it needs the internet, and the first connection
  waits for it.
- **Machine actions put the user name and home into `sh -c` and `.nspawn`
  files unquoted.** These values come only from the Mac user. Start/stop
  don't re-validate the machine name.
- **Docker/Podman API calls have no read timeout** (`docker.rs`): a hung
  `podman.service` stalls the domain sync.
- **mDNS:** no cap on the number of questions per query; the goodbye socket
  briefly shares port 5353; names cached under a wildcard get no goodbye.
- **`web.rs` doesn't cache a failed port detection:** each request to a
  container with no web server re-probes every port.
- **`hostnet.rs` never retries if binding port 2401 fails at boot.**

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
  - an absurd `memory_mib` wraps around;
  - the ssh `ProxyCommand` path isn't quoted, so a home folder with spaces
    breaks it;
  - a Docker binding only on `::1` maps to 127.0.0.1.

- **`~/Berth` (NFS) trusts its client.** Any local user on the Mac can
  mount the VM's export and gets root access to volumes and machines. Fine
  on a single-user Mac; the export only accepts the Mac's VM-network address.

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
- **The helper install runs NSAppleScript on the main thread**, so the app
  freezes during the password prompt.
- **The log view re-measures the whole text on every chunk**, which can
  beachball with chatty containers. Batch updates, or use an NSTextView.
- **Terminal sessions:** the exec session's fields are unsynchronized, and a
  quick tab switch during the exec upgrade can leak a thread and an fd.
  Sessions also end on every tab switch.
- **Follow streams:** a stream cancelled before its socket is set keeps a
  thread blocked.
- **Docker archive endpoint:** file view and download fail on PAX headers
  (long names, xattrs).
- **`TLSTrust.cached` is written off the main actor.**
- **UI:**
  - pull "Cancel" only hides the sheet;
  - container Delete doesn't ask for confirmation;
  - logs aren't re-attached after a restart;
  - a failed file preview replaces the table.
