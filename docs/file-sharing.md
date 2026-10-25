# File sharing

How the VM sees your Mac's home folder, and how to work on it.

## Which share

| macOS | Home folder served by | Setting |
|---|---|---|
| 27+ | berth's own virtio-fs device (default) | `"file_sharing": "berth"` in `~/.berth/config.json` |
| 27+ | Apple's virtiofs | `"file_sharing": "apple"`, or `BERTH_FILE_SHARING=apple berth start` |
| before 27 | Apple's virtiofs | – |

`berth status` shows which is in use (`Files:`). Either way the share has the
tag `home` and is mounted at your home path, so `docker run -v
~/project:/app` sees the same paths as on the Mac.

Three more folders are shared the same way and mounted by the agent after
boot, at the same paths: `/Volumes` (external drives), `/private/tmp` and
`/private/var/folders` (`$TMPDIR`). On the Mac `/tmp` and `/var/folders` are
symlinks into `/private`, which the Docker CLI passes on unresolved, and in
the VM `/tmp` is the VM's own: so berth's Docker socket bridge
(`src/dockerapi.rs`) rewrites bind sources under `/tmp` and `/var/folders`
in container-create requests to their `/private` paths. File change
forwarding (hot reload) covers all four.

Apple's share: no control over caching (every lookup crosses to the Mac),
and files show as root-owned in containers, so tools that set group
ownership (`rsync -g`, DDEV's `composer create-project`) fail.

## berth's share

macOS 27 added `VZCustomVirtioDeviceConfiguration`: a VM device whose
virtqueues you serve yourself. berth presents a virtio-fs device and answers
the Linux FUSE protocol from its own process.

```
guest app ─ syscall ─ Linux FUSE ─ virtio-fs driver ═ virtqueue ═ Virtualization.framework
                                                                       │ (device queue)
                                         src/virtiofs.rs  ── poll loop ┘
                                         src/fuse.rs      ── passthrough to APFS
```

- **`src/fuse.rs`**: a passthrough FUSE server.
  - Node IDs are inode numbers, and nodes are path-keyed.
  - Errno values and open flags are translated from macOS to Linux (arm64's
    values).
  - It runs in "no-open" mode: the guest kernel skips OPEN/RELEASE, and
    per-node open files are cached, up to 4096.
  - Likewise "no-opendir": no OPENDIR/RELEASEDIR, and the guest kernel
    keeps directory listings in its page cache until the directory's mtime
    changes (FOPEN_CACHE_DIR, its default without OPENDIR). Drupal's
    extension scan during `drush site:install` went from ~650k requests to
    ~250k.
  - The node table is in 64 shards by node ID: parallel requests (Composer
    unzips in parallel) don't queue on one lock, and a directory rename's
    walk over every node holds one shard at a time.
  - It replies ESTALE when the guest refers to something removed on the Mac,
    so the kernel looks it up again.
- **`src/fuse/nofollow.rs`**: every call refuses symlinks in any path
  component (`O_NOFOLLOW_ANY` and friends, or the parent directory plus
  `*at` calls). A container swapping a directory for a symlink can't lead
  the server outside the share.
- **`src/virtiofs.rs`**: the macOS 27 classes, declared by hand, and the
  device's event loop on its serial dispatch queue.
  - It polls the virtqueues for 100 µs after they empty, because
    notifications take ~70 µs to arrive.
  - With the polling driver it serves concurrent requests on worker threads.
  - It saves and restores the server's state for VM snapshots.
- **`src/watch.rs`**: forwards Mac-side changes into the guest.
  - The guest agent re-reads each changed path's attributes from the Mac
    (`statx` with `AT_STATX_FORCE_SYNC`), then `chmod`s it to its current
    mode, which raises inotify events. Re-reading first matters: a watcher
    woken by the event must already see the new size and contents.
  - Changes the guest made itself aren't echoed back.

### The polling guest driver

Most of a request's time was the framework delivering the reply interrupt
back into the guest. The polling driver is Linux's `fs/fuse/virtio_fs.c`
with three changes (marked `berth:`): built into berth's kernel (7.2,
`guest/kernel/virtiofs_poll-7.2.patch`), or as a module on Debian's 6.12
(`guest/kmod/virtiofs_poll/`, a copy of 6.12's file):

1. **Its own device ID.** It binds only device ID 62 and registers the
   filesystem `virtiofs_poll`, so the built-in driver keeps serving
   anything else.
2. **Polling.** A foreground request turns the queue's interrupt off and
   polls for its reply for up to `poll_us` (default 200 µs) before falling
   back to the interrupt.
   - Only requests sent by `__fuse_request_send()` poll: not background
     ones, which can be sent holding `fc->bg_lock`.
   - Debian's kernel has no preempt count, so `preemptible()` can't be used
     as the guard.

3. **No-op chmods answered in the guest.** A `chmod` to the mode a file
   already has, while its cached attributes are valid (not invalidated, size
   not changing), is answered from the inode without a round trip
   (`skip_noop_chmod`, counter `noop_chmods`). Composer chmods every file it
   writes two or three times.

A round trip went from ~180 µs to ~18 µs (Apple's share: ~34 µs).

### How it's set up (nothing to do by hand)

On berth's kernel (the default) the driver is built in: the host presents
device ID 62 from the first start, and the steps below don't apply. The
polling-driver marker (below) still guards each start. On Debian's kernel:

1. **First boot on berth's share.** The host presents the device as plain
   virtio-fs (ID 26), and the guest's built-in driver mounts it from
   `/etc/fstab` (`home <home> virtiofs`).
2. **The agent prepares the polling driver** (`agent/src/kmod.rs`):
   - installs `/usr/sbin/mount.berth` and switches the fstab line to type
     `berth`;
   - fetches the kernel headers and a compiler, and builds the module from
     the source embedded in the agent;
   - writes `~/.berth/guest/kmod-ready` naming the kernel.
3. **Later starts.** When `kmod-ready` matches the kernel about to boot
   (`~/.berth/guest/boot/release`), the host presents device ID 62.
   `mount.berth` then mounts with `virtiofs_poll`, or falls back to
   `virtiofs`.
4. **After a kernel update** the IDs don't match, so it falls back to step 1
   and the agent rebuilds the module.

**Safety nets.** The agent runs from the home folder, so a share that fails
to mount would leave a VM that never comes up:

- **Polling-driver marker:** before using ID 62 the host leaves
  `boot/kmod-trying`, which the agent clears once it's up. If it's still
  there at the next start, that start uses ID 26.
- **Watchdog:** if the agent doesn't answer within 90 s of boot (not on
  first boot), the daemon writes `boot/safe-boot` and stops.
- **Safe-mode retry:** `berth start` then retries once in safe mode. Home is
  on Apple's share, and the kernel command line mounts it directly
  (`fstab=0 systemd.mount-extra=…`, root read-write).
- **Repair:** in safe mode the agent puts the fstab line back to plain
  `virtiofs` and leaves the helper unused until a berth with a different
  helper arrives.
- **Snapshots:** restoring a snapshot clears `kmod-ready`, since its disk
  may predate the module (on Debian's kernel only; berth's has it built in).

### Settings (environment of `berth start`)

| Variable | Default | |
|---|---|---|
| `BERTH_FILE_SHARING` | `berth` on macOS 27 | `apple` to use Apple's share |
| `BERTH_FS_POLL_DRIVER` | automatic | `1`/`0` forces device ID 62/26 |
| `BERTH_FS_ENTRY_TIMEOUT`, `BERTH_FS_ATTR_TIMEOUT` | 1 | seconds the guest may cache names / attributes |
| `BERTH_FS_NEGATIVE_TIMEOUT` | 1 | seconds it may remember a name doesn't exist |
| `BERTH_FS_POLL_US` | 100 | host-side polling of idle queues |
| `BERTH_FS_THREADS` | on with the polling driver | serve concurrent requests on GCD worker threads |
| `BERTH_FS_QUEUES` | 1 | request queues (more didn't help) |
| `BERTH_FS_NO_OPENDIR` | on | `0` keeps OPENDIR/RELEASEDIR (and uncached listings) |
| `BERTH_FS_WRITEBACK` | off | FUSE write-back cache (adds requests) |
| `BERTH_FS_STATS` | off | per-request timings and per-opcode counts in `daemon.log` |
| `BERTH_FS_TRACE` | off | log every request (opcode, node, name) |

Guest side: `/sys/module/virtiofs_poll/parameters/poll_us` (writable), and
the counters `poll_hits`, `poll_misses`, `poll_skipped` and `poll_hit_ns`
(`bench/fs/poll-stats.sh`).

### Measuring

```sh
bench/fs/make-tar.sh ~/.berth-fsbench                 # 20,000 x 1 KB files
bench/fs/run.sh berth                                 # the default share
bench/fs/run.sh apple BERTH_FILE_SHARING=apple
berth shell -- python3 $PWD/bench/fs/latency.py ~/.berth-fsbench/small.tar   # with 0 timeouts, see the script
bench/ddev-drupal.sh <label>                          # DDEV + Drupal CMS, see bench/results
```

Results (M2 Max, seconds; `bench/fs` runs and `bench/results/2026-10-23-ddev-drupal.md`,
DDEV re-measured on 2026-10-24, `bench/results/2026-10-24-compare.md`):

| | Apple's share | berth, built-in driver | berth, polling driver (default) | OrbStack 2.2.5 |
|---|---|---|---|---|
| extract 20k files | 7.5 | 7.3 | **2.4** | 2.3 |
| delete 20k files | 2.7 | 2.7 | **1.4** | 1.5 |
| read all 20k files (`compare.sh`) | 2.9 | – | 2.0 | 0.8 |
| 4 parallel extracts | 20.4 | 12.8 | 7.7 | – |
| DDEV `composer install` | fails* | 16 | 8.2–8.9 | 6.6 |
| DDEV `drush site:install` | fails* | 67 | 31.7–36.3 (berth's 7.2 kernel) | 37 |

\* `create-project` aborts on Apple's share (ownership), so the later steps
can't run.

### Known limits

- Reading many small files is ~2.4x slower than OrbStack: each file still
  costs a lookup, an attribute fetch and a read round trip.
- Composer is still ~1.3x behind OrbStack. It sends ~10 requests per file
  (the guest kernel and Composer decide that), among them a create and a
  timestamp set that must reach APFS, which barely scales with threads:
  creating a file costs ~55 µs on one thread and ~41 µs on eight
  (2026-10-24), and ~99 µs under Composer's parallel writes.
- `drush site:install` (on OrbStack too) takes either ~35 s or ~45 s from
  run to run; compare like with like.
- Cache coherency relies on FSEvents: a change on the Mac reaches the guest
  in ~0.6 s. Until then the guest may use cached names, for at most the
  entry timeout.
- More in `docs/KNOWN_ISSUES.md`.
