# DDEV + Drupal CMS: OrbStack vs berth

2026-10-23, MacBook Pro M2 Max, macOS 27.0, DDEV 1.25.2, Drupal CMS 2.2.7
(38,921 files). `bench/ddev-drupal.sh`: bind mounts (performance mode `none`, no
Mutagen), pages fetched from the web container's IP with a random query string
so every request runs PHP. Warm page = median of 20 after 10 warm-up requests.

| | ddev start | create-project | composer install (cached) | drush site:install | first page | warm page | restart |
|---|---|---|---|---|---|---|---|
| OrbStack 2.2.5 | 10.8–11.8 s | **10.7–11.0 s** | **6.6 s** | **36.7–37.4 s** | **2.0 s** | 14 ms | **20.8–21.1 s** |
| berth, own virtio-fs (experimental) | 12.4–12.8 s | 21.8–23.0 s | 16.0–16.1 s | 66.5–68.1 s | 4.2–4.4 s | 17–18 ms | 24.1–24.5 s |
| berth, own virtio-fs + polling guest driver (experimental, 2026-10-24) | 11.4–14.2 s | 14.6–15.5 s | 10.3 s | 44.0–44.2 s | 2.7–2.8 s | 18–21 ms | 23.6–24.1 s |
| berth, own virtio-fs + polling driver, default (2026-10-24, later) | 11.3–12.9 s | 13.5–14.1 s | **8.9–9.2 s** | 38.9–40.7 s | 2.6–2.7 s | 21–29 ms | 26.9–28.0 s |
| berth, polling driver, with Mac-side delete replay (2026-10-24, 3 runs) | 9.3–14.3 s | 14.0–17.0 s | **8.5–8.8 s** | 41.4–44.8 s | 2.4–2.5 s | 16–18 ms | 25.6–26.2 s |
| berth, no-opendir listing cache, sharded node table (2026-10-24, 3 runs) | 9.8–14.0 s | **13.4–15.3 s** | **8.5–9.0 s** | **35.2–36.1 s** (46.7 slow mode) | 2.3–2.6 s | 16–18 ms | 24.5–28.6 s |
| berth's own kernel 6.12.113-berth (2026-10-24, 3 runs) | 11.5–15.1 s | 13.3–15.8 s | 8.6–8.7 s (10.1 first after boot) | 35.3–37.8 s (41.0) | 2.2–2.7 s | 17 ms | 23.7–26.4 s |
| berth's kernel 7.2.12-berth + memory/boot tuning (2026-10-25, 3 runs) | 8.9–12.6 s | 12.9–13.4 s | 8.2–8.9 s | **31.7–36.3 s** | **1.9–2.1 s** | 17–21 ms | 22.9–26.2 s |
| berth 7.2.12, one poller per queue, ioctls in the guest (2026-10-25, 2 runs, busy Mac) | 9.0–14.5 s | 13.6–15.2 s | 7.3–7.6 s | 33.9 s (50.4) | 1.8 s | 16–17 ms | 22.9–24.9 s |
| berth, Apple's virtiofs | 13.2 s | fails | – | – | – | – | – |

- On Apple's share the project folder appears owned by root inside the
  container, so DDEV's `rsync -g` into it fails (`chgrp: Operation not
  permitted`) and `ddev composer create-project` aborts. berth's own share
  reports real ownership and accepts the change.
- Once warm, page rendering is within a few ms of OrbStack. Everything that
  writes many small files is 2–2.4x slower: each file costs several round trips
  through the framework's custom Virtio device (~70 µs each, versus ~15 µs to
  serve), which polling the queues only halves.
- The experimental share was bind-mounted over `~/ddev-bench` only, with 10 s
  entry/attribute caching and 1 s negative caching
  (`BERTH_EXPERIMENTAL_FS=1 BERTH_FS_ENTRY_TIMEOUT=10 BERTH_FS_ATTR_TIMEOUT=10`).
- Found and fixed while running this: lost truncation on `open(O_TRUNC)`
  (a rewritten shorter `composer.json` kept its old tail), stale cached names
  after `rm -rf vendor` on the Mac (now ESTALE, so the guest re-looks up), and a
  descriptor leak per created file.

## 2026-10-24: polling guest driver

`guest/kmod/virtiofs_poll` (`BERTH_FS_POLL_DRIVER=1`, default 1 s caching)
spins for replies instead of waiting for the framework's interrupt: a
request's round trip went from ~180 µs to ~18 µs (Apple's share: ~34 µs).
On 20k x 1 KB files berth now extracts in 2.3 s and deletes in 1.3 s, on par
with OrbStack (2.27 / 1.48 s). Composer is still ~1.5x slower: it's parallel
and APFS serializes metadata changes (more threads only make each one
slower), and every file costs ~10 requests.

Also fixed on the way: berth's file watcher echoed the guest's own writes
back into it (a chmod round trip per file), and a deletion on the Mac right
after the guest touched a path could be missed.

## 2026-10-25: kernel 6.12 vs 6.18 vs 7.2

berth's kernel built from each (same config, `guest/kernel`), with the
same boot and memory tuning, fresh boot and 5 minutes' settling before
idle CPU, 3 DDEV runs each:

| kernel | kernel boot | start to Docker ready | `check.sh` + k3s | idle CPU | drush site:install | first page |
|---|---|---|---|---|---|---|
| 6.12.113 | 0.31 s | 2.72–2.84 s | pass | 1.32–1.42 % | 35.9–44.5 s | 2.3–2.7 s |
| 6.18.58 | 0.32 s | 2.64–2.73 s | **VM froze** (hard lockup, twice) | – | – | – |
| 7.2.12 | **0.24 s** | 2.64–2.71 s | pass | 1.37–1.45 % | **31.7–36.3 s** | **1.9–2.1 s** |

6.18 locked up a CPU ~48 s after boot both times, as k3s tore down its pods'
veths; no backtrace was printed. 7.2 is the default (OrbStack ships 7.2.12
too). Idle CPU read right after DDEV is 4–9 %: the balloon is still handing
memory back to macOS for a few minutes, so measure after it settles.
