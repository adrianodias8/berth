# File sharing

How berth shares your Mac's files with the VM, the experimental faster path
it's building, and how to work on it.

## Today's default: Apple's virtiofs

`src/vm.rs` adds Apple's `VZVirtioFileSystemDeviceConfiguration` with the tag
`home`, mounted in the guest at your home path, so `docker run -v
~/project:/app` works with the same paths as on the Mac. It is simple and
stable, but:

- every request costs ~450 µs of transport and service; small-file work is
  ~3.5x slower than OrbStack;
- there is no say over caching;
- inside containers your files show up root-owned, so tools that set group
  ownership (`rsync -g`, DDEV's `composer create-project`) fail with
  `Operation not permitted`.

## berth's own share (experimental, macOS 27)

macOS 27 added `VZCustomVirtioDeviceConfiguration`: a VM device whose
virtqueues you serve yourself. berth presents a virtio-fs device (device ID 26,
tag `berthfs`) and answers the Linux FUSE protocol from its own process.

```
guest app ─ syscall ─ Linux FUSE ─ virtio-fs driver ═ virtqueue ═ Virtualization.framework
                                                                       │ (device queue)
                                         src/virtiofs.rs  ── poll loop ┘
                                         src/fuse.rs      ── passthrough to APFS
```

- `src/fuse.rs` is a passthrough FUSE server: node IDs are inode numbers,
  nodes are path-keyed, errno and open flags are translated from macOS to
  Linux. It runs in "no-open" mode (the kernel skips OPEN/RELEASE; per-node
  open files are cached, capped at 4096) and replies ESTALE when the guest
  refers to something removed on the Mac, so the kernel looks it up again.
- `src/virtiofs.rs` declares the macOS 27 classes by hand (they're not in
  objc2-virtualization yet), runs the device's event loop on its serial
  dispatch queue, and polls the virtqueues for 100 µs after they empty.
  Notifications from the framework take ~70 µs to arrive, so polling skips
  them while the guest is busy.
- `src/watch.rs` forwards Mac-side changes into the guest: the guest agent
  `chmod`s each changed path to its current mode, which refreshes the
  guest's cached attributes and raises inotify events. The file server
  records the state its own changes left, so the guest's writes aren't echoed
  back to it.

### The polling guest driver

Most of a request's time was the framework delivering the reply interrupt
back into the guest. `guest/kmod/virtiofs_poll/` is Linux 6.12's
`fs/fuse/virtio_fs.c` with two changes (marked `berth:`):

1. it binds only device ID 62 and registers the filesystem `virtiofs_poll`,
   so the built-in driver keeps serving Apple's shares;
2. a foreground request turns the queue's interrupt off and polls the used
   ring for its reply for up to `poll_us` (default 200 µs) before falling
   back to the interrupt.

With it a round trip went from ~180 µs to ~18 µs (Apple's share: ~34 µs).

Only requests sent by `__fuse_request_send()` poll: not background ones (they
can be sent holding `fc->bg_lock`) and not ones without a reply. The guest
kernel has no preempt count, so `preemptible()` can't be used as the guard.

### Try it

```sh
./scripts/build.sh
berth stop
BERTH_EXPERIMENTAL_FS=1 berth start                   # built-in driver, device ID 26
berth shell -- sudo $PWD/bench/fs/guest-mount.sh      # mounts /mnt/berthfs

# With the polling driver (built once per guest kernel):
berth shell -- sudo $PWD/bench/fs/guest-kbuild.sh $PWD/guest/kmod/virtiofs_poll
berth stop
BERTH_EXPERIMENTAL_FS=1 BERTH_FS_POLL_DRIVER=1 berth start
berth shell -- sudo $PWD/bench/fs/guest-mount.sh ddev-bench   # also binds ~/ddev-bench
berth shell -- sh $PWD/bench/fs/poll-stats.sh
```

The share's root is your home folder. Mounting it over the whole home
(replacing Apple's share) is the planned default but untested; bind just a
project folder over its own path, as `guest-mount.sh <folder>` does.

### Settings (environment of `berth start`)

| Variable | Default | |
|---|---|---|
| `BERTH_EXPERIMENTAL_FS` | off | add the device |
| `BERTH_FS_POLL_DRIVER` | off | present it as device ID 62, for `virtiofs_poll` |
| `BERTH_FS_ENTRY_TIMEOUT`, `BERTH_FS_ATTR_TIMEOUT` | 1 | seconds the guest may cache names / attributes |
| `BERTH_FS_NEGATIVE_TIMEOUT` | 1 | seconds it may remember a name doesn't exist |
| `BERTH_FS_POLL_US` | 100 | host-side polling of idle queues |
| `BERTH_FS_THREADS` | on with the polling driver | serve concurrent requests on GCD worker threads |
| `BERTH_FS_QUEUES` | 1 | request queues (more didn't help) |
| `BERTH_FS_WRITEBACK` | off | FUSE write-back cache (adds requests) |
| `BERTH_FS_STATS` | off | per-request timings and per-opcode counts in `daemon.log` |
| `BERTH_FS_TRACE` | off | log every request (opcode, node, name) |

Guest side: `/sys/module/virtiofs_poll/parameters/poll_us` (writable), and
counters `poll_hits`, `poll_misses`, `poll_skipped`, `poll_hit_ns`.

### Measuring

```sh
bench/fs/make-tar.sh ~/.berth-fsbench                 # 20,000 x 1 KB files
APPLE=1 bench/fs/run.sh builtin                       # Apple's share, then berth's
bench/fs/run.sh poll BERTH_FS_POLL_DRIVER=1
berth shell -- python3 $PWD/bench/fs/latency.py /mnt/berthfs/.berth-fsbench/small.tar
bench/ddev-drupal.sh <label>                          # DDEV + Drupal CMS, see bench/results
```

Results so far (M2 Max, seconds; `bench/results/2026-10-23-ddev-drupal.md`):

| | Apple's share | berth, built-in driver | berth, polling driver | OrbStack 2.2.5 |
|---|---|---|---|---|
| extract 20k files | 7.8 | 7.3 | **2.3** | 2.3 |
| delete 20k files | 2.6 | 2.7 | **1.3** | 1.5 |
| 4 parallel extracts | 19.1 | 12.8 | 7.5 | – |
| DDEV `composer install` | fails* | 16 | 10.3 | 6.6 |
| DDEV `drush site:install` | fails* | 67 | 44 | 37 |

\* `create-project` aborts on Apple's share (ownership), so later steps
can't run.

### Status and what's left

- Experimental: off unless enabled, and the module is built by hand.
- To make it the default: build or ship the module for the guest kernel
  (the agent could build it, or berth could ship a prebuilt `.ko` per Debian
  kernel version), mount the share at the home path in place of Apple's,
  load the module early (before Docker starts), and test widely.
- Composer is still ~1.5x behind OrbStack. It sends ~10 requests per file
  (the guest kernel and Composer decide that), its unzip runs in parallel,
  and APFS serializes metadata changes, so more threads only make each one
  slower. Creating a file on APFS alone costs ~53 µs.
- Cache coherency relies on FSEvents: a change on the Mac reaches the guest
  in ~0.6 s; until then the guest may use cached names (at most the entry
  timeout). Linux's virtio-fs has no notification queue for pushing
  invalidations; the polling driver could add one.
