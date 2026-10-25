# Changelog

berth has no releases yet; this lists what changed, newest first. Details
are in the commit messages.

## Unreleased

### 2026-10-25
- **Fixes:** disks under `/Volumes` no longer share file IDs (two disks'
  root folders were both inode 2); a stop during boot no longer counts as a
  failed kernel start; a rebuilt kernel is tried again after an earlier one
  failed; a new VM's first boot skips building the file driver that
  berth's kernel has built in; restoring a saved state clears all boot
  markers.
- **Composer ~10 % faster, now ~1.1x OrbStack** (Drupal CMS `composer
  install` 8.5–9.0 -> 7.2–7.7 s, OrbStack 6.5–7.0 s). The polling driver
  lets one CPU poll a queue at a time (eight threads creating files: 1.98 ->
  0.93 s, OrbStack 0.75 s) and answers ioctls itself (musl's `fopen()`
  asked each file it wrote whether it was a terminal); the host closes
  files on a thread of its own.
- **Idle memory ~0.5 GB lower:** macOS indexed `~/Berth` each time it was
  mounted, walking every Docker volume over SMB and leaving the VM's Samba
  with a 600 MB heap; the share now opts out of indexing. `berth status`
  no longer claims memory was "returned to macOS": Virtualization.framework
  keeps what the guest touched (see KNOWN_ISSUES), the balloon only limits
  it. The memory benchmark's workload was fixed (it stopped at 2 GiB).
- **App:** process runs, control-socket and Docker requests no longer
  block Swift's shared thread pool (one thread per core), so a long
  `berth start`, machine action or engine install can't hold up the rest
  of the app.
- **Small-file reads 2x faster:** readahead is no longer sent as background
  requests with the polling driver (each waited for an interrupt), and files
  are opened read-only until written. 20k files read in 0.75 s instead of
  1.6 s (OrbStack 0.63 s); cold Jest start 0.48 -> 0.43 s (OrbStack 0.36 s).
- **Guest kernel:** berth boots its own kernel, Linux 7.2.12 with what the
  VM needs built in and no initrd (`guest/kernel`): kernel boot 0.24 s
  instead of Debian's 0.59 s, the polling file driver built in (no runtime
  build, no download). Falls back to Debian's kernel after two failed
  starts; `berth config set kernel debian` opts out. Chosen over 6.12 and
  6.18 by measurement (6.18 locked up under k3s).
- **Memory and power tuning** (from OrbStack's running kernel): zram swap,
  swappiness 20, huge pages on request, no guest disk scheduler,
  `workqueue.power_efficient`, no per-container kernel-memory accounting.
- **Idle power:** the VM no longer wakes for Podman's event stream or the
  app's Docker pings; idle CPU 2.97 % -> ~1.4 %.
- **File sharing:** directory listings cached in the guest and a sharded
  node table (`drush site:install` requests 650k -> 250k, now on par with
  OrbStack); watchers see files deleted on the Mac (IN_DELETE); `berth
  status` and the app say when the share is on the slower built-in driver.
- **Known-issue fixes:** helper routes only on VM bridges, pinned CLI
  checksums, `::1` published ports, connection limits in the agent, atomic
  DNAT updates, mDNS limits; in the app, PAX archives, a stream cancel race,
  snapshot busy state, terminals that survive tab switches.
- **README:** quick start and screenshots.

### 2026-10-24
- **Kubernetes:** LoadBalancer services on the Mac's localhost.
- **Machines:** isolated machines (`--isolated`, `--mount`,
  `--isolate-network`; systemd distributions), and `restart`, `rename`,
  `info`, `logs`, `push`, `pull`; in the app too, except push/pull.
- **Ports:** `"ports": "localhost"` keeps published ports off the LAN.
- **CLI:** `berth open`, `berth status` exits 1 when stopped, `machine
  delete` asks first.
- **Security:** isolated machines are entered only through their own
  systemd, their network rules apply before they start, and files berth
  touches inside machines resolve inside them; `~/Berth` no longer follows
  links out of the share.
- **Finder:** running containers' root filesystems at `~/Berth/containers`;
  `~/Berth` is now SMB with a password (no helper needed), files show as yours.
- **App:** import from OrbStack or Docker Desktop (Settings), and Intel /
  cloud-init options for new machines.
- **CLI:** `berth restart`, `berth info`, `berth logs`, `berth completions`.
- **Certificates:** Docker in the VM trusts the CAs the Mac trusts.
- **More of the Mac in containers:** `/Volumes`, `/tmp` and `$TMPDIR`
  (`/var/folders`) bind mounts work, at the same paths.
- **Fixed:** files created with O_EXCL|O_NOFOLLOW on bind mounts (busybox
  tar's extraction) failed with EINVAL on berth's share.
- **Review fixes:** ~/.ssh/config is never replaced when it can't be read and
  is edited through symlinks (with a backup); `berth import` keeps anonymous
  volumes' data, doesn't copy volumes that are Mac folders, and reports
  failures; `mac` sessions hang up their command when the machine side goes
  away; ~/Berth got a stricter mount (replaced the same day by SMB);
  proxy changes never restart Docker; machines autostart reliably at boot;
  the app merges its settings into config.json; fewer idle forks in the VM.

- **Machines and the Mac:** `ssh <machine>.berth` (VS Code Remote-SSH), the
  `mac` command inside machines, and the Mac's SSH agent in machines and at
  `/run/host-services/ssh-auth.sock`.
- **Releases:** `scripts/release.sh` and a tag-triggered GitHub workflow
  build a Developer ID-signed, notarized Berth.app zip (ad-hoc without a
  certificate).
- **Machines:** `--arch amd64` (Intel distros on Rosetta) and
  `--cloud-init <user-data>`; machines resolve their own hostname.
- **Kubernetes:** services resolve on the Mac as
  `<name>.<namespace>.svc.cluster.local`.
- **Settings:** `berth config list|get|set|unset`, with values checked.
- **Proxy:** Docker's pulls and pushes follow the Mac's proxy settings (or a
  given URL); a change applies at the next start, never restarting Docker
  under containers, builds or pulls.
- **Import:** `berth import orbstack|docker-desktop` copies images, volumes,
  networks and containers (optionally one Compose project).
- **Alpine machines** stop with `berth machine stop` and with the VM (busybox
  init gets SIGUSR2, not systemd's signal).
- **Security:** the guest agent only takes orders from the Mac (vsock CID 2).
- **Finder:** Docker volumes and machines at `~/Berth` (NFS at first, SMB since).
- **Debug shell:** `berth debug <container>`, a shell with the VM's tools in
  any container's namespaces, on a throwaway overlay.
- **File sharing:** berth's own virtio-fs device is the default on macOS 27,
  with a polling guest driver (`guest/kmod/virtiofs_poll`) that brings a
  request from ~180 µs to ~18 µs. The driver answers no-op chmods itself.
  Hot reload no longer misses a Mac-side edit that lands while the guest's
  attributes are cached.
- **Snapshots:** `berth snapshot create|list|restore|delete` save the whole
  VM as copy-on-write clones, with memory while the Mac is unlocked.
  Snapshots keep the VM's identity and are excluded from Time Machine.
- **Suspend:** `berth stop --suspend` (or `"stop": "suspend"`) saves the VM
  and the next start resumes it in ~3 s. By default `berth stop` shuts down,
  like OrbStack. Changed settings make the next start boot instead of
  resuming.
- **host.docker.internal** runs over berth's own tunnel device on macOS 27:
  ~21 Gbit/s Mac to container (6.9 over vsock). Idle connections keep the
  agent killable; past 48 connections new ones use vsock.
- **Host networking:** `--net host` servers appear on the Mac's localhost,
  and only there.
- **IPv6** for containers, opt-in.
- **Ports:** forwards are synced one at a time, and ports busy on the Mac are
  retried every 5 s.
- **Clock:** the guest's clock follows the Mac's (NTP-style, slewed under a
  second), including after sleep and restores.
- **Robustness:** a watchdog with a safe-mode retry for broken mount setups,
  which leaves an interrupted first boot alone; snapshots and suspend are
  serialized.
- Docs: configuration, troubleshooting (with uninstall), file sharing,
  known issues, learnings, contributing; MIT license.

### 2026-10-23

- First version: Debian 13 VM on Virtualization.framework with Docker
  (docker-ce 29), booted straight into its kernel (Docker ready ~3 s after
  `berth start`), ASIF disks, a memory balloon, Rosetta.
- Published ports (TCP and UDP), container IPs from the Mac (root route
  helper), `host.docker.internal`, `*.berth.local` domains over mDNS with
  HTTP/HTTPS on a local CA.
- Bundled Docker, Compose, Buildx and kubectl CLIs; Kubernetes (k3s); Linux
  machines; Podman and nerdctl next to Docker.
- File change forwarding for hot reload.
- Berth.app: menu bar, containers, images, volumes, networks, Kubernetes,
  machines, terminals, logs, files, stats, first-run setup.
- Benchmarks against OrbStack (`bench/`).
