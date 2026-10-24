# Configuration

berth reads `~/.berth/config.json` when the VM starts. The app's Settings
write the same file, so you rarely need to edit it. A missing file or key
means the default. Changes apply on the next start (`berth stop --shutdown
&& berth start`, or Restart in the app).

```json
{
  "cpus": 8,
  "memory_mib": 12288,
  "file_sharing": "berth",
  "ipv6": false,
  "stop": "shutdown",
  "finder": true
}
```

| Key | Default | |
|---|---|---|
| `cpus` | every core | CPUs the VM may use, clamped to what Virtualization.framework allows |
| `memory_mib` | half the Mac's RAM, 2–16 GiB | the most memory the VM may hold. The balloon returns what the guest doesn't use, so this is a ceiling, not a reservation |
| `file_sharing` | `"berth"` on macOS 27 | `"apple"` uses Apple's virtiofs for the home folder (`docs/file-sharing.md`). Before macOS 27 it's always Apple's |
| `ipv6` | `false` | IPv6 subnets for Docker networks, NAT'd out of the VM. Changing it restarts dockerd, which stops running containers |
| `stop` | `"shutdown"` | `"suspend"`: `berth stop` saves the VM's state and the next start resumes it (see below) |
| `finder` | `true` | Docker volumes and machines' files in Finder at `~/Berth`, over NFS from the VM, while berth runs |

## Suspend instead of shutdown

With `"stop": "suspend"` (or `berth stop --suspend`), `berth stop` pauses
the VM and saves its memory to `~/.berth/data/suspended/`. The next `berth
start` resumes it in about 3 s, with containers still running.

- macOS saves a VM's state only while the Mac is unlocked. If it can't, the
  VM shuts down instead.
- If the CPUs, memory or file sharing setting changed since, the next start
  boots the saved disk instead of resuming, so the settings apply. That's
  like booting after a power cut: containers are restarted, not resumed.
- A state saved on another macOS build may not resume. berth then boots the
  disk the same way.
- `berth stop --shutdown` always shuts down, and the app's Restart uses it.
- The saved state is as large as the memory the VM was using.

## Environment variables

These are for testing and debugging; set them for `berth start`.

| Variable | |
|---|---|
| `BERTH_FILE_SHARING` | `apple` or `berth`; overrides `file_sharing` |
| `BERTH_FS_POLL_DRIVER` | `1`/`0`: force the polling guest driver on or off (normally automatic) |
| `BERTH_FS_ENTRY_TIMEOUT`, `BERTH_FS_ATTR_TIMEOUT`, `BERTH_FS_NEGATIVE_TIMEOUT` | seconds the guest may cache names, attributes and missing names (default 1) |
| `BERTH_FS_POLL_US` | how long the host polls idle queues, in µs (default 100) |
| `BERTH_FS_THREADS`, `BERTH_FS_QUEUES`, `BERTH_FS_WRITEBACK` | file server tuning, see `docs/file-sharing.md` |
| `BERTH_FS_STATS`, `BERTH_FS_TRACE` | `1`: per-request timings, or every request, in `daemon.log` |

## Files

| Path | |
|---|---|
| `~/.berth/config.json` | this file |
| `~/.berth/data/` | the VM: disk image, EFI variables, machine identity, a suspended state |
| `~/.berth/snapshots/` | snapshots (excluded from Time Machine) |
| `~/.berth/guest/` | the guest agent, the exported kernel, boot markers |
| `~/.berth/bin/` | the bundled `docker`, Compose, Buildx and `kubectl` |
| `~/.berth/run/` | sockets: `docker.sock`, `podman.sock`, `ssh.sock`, the control socket |
| `~/.berth/tls/` | the local CA for `https://*.berth.local` |
| `~/.berth/log/` | `daemon.log` (the host side) and `console.log` (the VM's console) |
| `~/.berth/ssh/` | the VM's SSH key, and `config` (`ssh berth`, `ssh <machine>.berth`), included from `~/.ssh/config` |
