# Small-file reads: 2026-10-25

Apple M2 Max, 32 GiB, macOS 27.0.1; berth's 7.2.12 kernel with the polling
driver; OrbStack 2.2.5. `bench/fs/in-container.sh` (timed inside the
container), 20,000 × 1 KB files on a bind mount from home.

| | extract | read all | delete |
|---|---|---|---|
| berth, async readahead (before) | 2.25–2.54 s | 1.43–1.67 s | 1.42–1.55 s |
| berth, readahead as ordinary requests | 2.29–2.49 s | **0.62–0.76 s** | 1.42–1.55 s |
| OrbStack | 2.10–2.17 s | 0.63 s | 1.24–1.26 s |

In the VM, after dropping its caches, with `find … -exec cat`: 1.8 s →
1.6 s from opening files read-only (a READ on the Mac 31 → 24 µs), then
0.75 s without async readahead.

The cause: during the slow read the polling driver's `poll_skipped` rose by
exactly 20,000, one per file. Readahead went out as background requests,
which the driver doesn't poll for, so each waited for an interrupt.

1 GiB on the same bind mount, guest caches dropped: one reader 0.35–0.38 s
before, 0.26–0.27 s after; four parallel 256 MiB readers 0.20–0.21 s both.

Node (10k-file `node_modules`, guest caches dropped, three runs):

| | jest (one test) | webpack --help | tsc --noEmit |
|---|---|---|---|
| berth before | 0.46–0.49 s | 0.15–0.16 s | 0.20–0.21 s |
| berth after | 0.42–0.46 s | 0.15–0.16 s | 0.19–0.20 s |
| OrbStack | 0.36–0.52 s | 0.14–0.17 s | 0.18–0.19 s |

DDEV + Drupal CMS (two runs, no settle): composer install 8.7–9.1 s, drush
site:install 36.2–41.5 s, first page 1.8–2.2 s, the same as before within
noise (PHP keeps compiled files in its opcache; Composer mostly writes).
