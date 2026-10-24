# Changelog

berth has no releases yet; this lists what changed, newest first. Details
are in the commit messages.

## Unreleased

### 2026-10-24
- **Kubernetes:** LoadBalancer services on the Mac's localhost.
- **More of the Mac in containers:** `/Volumes`, `/tmp` and `$TMPDIR`
  (`/var/folders`) bind mounts work, at the same paths.
- **Fixed:** files created with O_EXCL|O_NOFOLLOW on bind mounts (busybox
  tar's extraction) failed with EINVAL on berth's share.
- **Review fixes:** ~/.ssh/config is never replaced when it can't be read and
  is edited through symlinks (with a backup); `berth import` keeps anonymous
  volumes' data, doesn't copy volumes that are Mac folders, and reports
  failures; `mac` sessions hang up their command when the machine side goes
  away; ~/Berth is mounted by the root helper and only from reserved ports;
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
- **Finder:** Docker volumes and machines at `~/Berth`, over NFS from the VM.
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
