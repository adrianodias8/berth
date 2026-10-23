# DDEV + Drupal CMS: OrbStack vs berth

2026-10-23, MacBook Pro M2 Max, macOS 27.0, DDEV 1.25.2, Drupal CMS 2.2.7
(38,921 files). `bench/ddev-drupal.sh`: bind mounts (performance mode `none`, no
Mutagen), pages fetched from the web container's IP with a random query string
so every request runs PHP. Warm page = median of 20 after 10 warm-up requests.

| | ddev start | create-project | composer install (cached) | drush site:install | first page | warm page | restart |
|---|---|---|---|---|---|---|---|
| OrbStack 2.2.5 | 10.8–11.8 s | **10.7–11.0 s** | **6.6 s** | **36.7–37.4 s** | **2.0 s** | 14 ms | **20.8–21.1 s** |
| berth, own virtio-fs (experimental) | 12.4–12.8 s | 21.8–23.0 s | 16.0–16.1 s | 66.5–68.1 s | 4.2–4.4 s | 17–18 ms | 24.1–24.5 s |
| berth, Apple's virtiofs (default) | 13.2 s | fails | – | – | – | – | – |

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
