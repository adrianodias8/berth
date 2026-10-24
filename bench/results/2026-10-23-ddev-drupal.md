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
