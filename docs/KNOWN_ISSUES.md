# Known issues

Open findings from the code review of 2026-10-24, most important first
within each area. Fixed findings aren't listed (see the git log). When you
fix one, remove it here in the same change.

## File sharing (experimental share, `src/fuse.rs`)

These block making berth's share the default.

- **Symlink races can escape the share.** Every operation works on a full
  path, and a symlink in any component is followed. A container that swaps a
  directory for a symlink between a LOOKUP and its use can get a node whose
  path points outside the bind mount (anything your user can reach, such as
  `~/.ssh`).

  Fix: keep a directory handle per node and use `*at` calls, with
  `O_NOFOLLOW_ANY` / `AT_SYMLINK_NOFOLLOW_ANY` (macOS 11+), and
  `renameatx_np(RENAME_NOFOLLOW_ANY)`. Canonicalize the root once.
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

## Host daemon (`src/`)

- **A failed port bind is retried only on the next container event**
  (`ports.rs`): if something on the Mac held the port, the forward stays
  missing until then. Retry failed ports on a timer.
- **Concurrent `ports::sync()` calls can apply an older snapshot last**,
  and each Docker event runs a full sync. Use one coalescing sync task, as
  the agent's domain sync now does.
- **`helper::bind_privileged` blocks a tokio worker** while holding the
  `FORWARDS` lock (up to a 5 s timeout).
- **Direct boot gets turned off** if berth stops after the "trying" marker
  is written but before the guest clears it.
- **Small edge cases:**
  - an absurd `memory_mib` wraps around;
  - the ssh `ProxyCommand` path isn't quoted, so a home folder with spaces
    breaks it;
  - a Docker binding only on `::1` maps to 127.0.0.1.
- **Anything in the VM can reach any loopback-only service on the Mac**
  through `host.docker.internal` (as with OrbStack), including the forwarded
  Kubernetes API on 6443. Worth stating in the README.

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
