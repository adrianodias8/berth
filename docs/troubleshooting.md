# Troubleshooting

Start with `berth status`: it shows the VM, Docker, file sharing, the IP
route helper and the forwarded ports. `berth info` adds versions, settings
and paths (paste it into a bug report), and `berth logs [-f] [console|helper]`
shows the logs, which are in `~/.berth/log/`:
`daemon.log` is the host side, `console.log` is the VM's console (kernel
and boot messages). `berth shell` opens a shell in the VM.

## berth doesn't start

- **First start.** It downloads Debian and installs Docker, which takes a
  few minutes and needs the internet. If it was interrupted, `berth start`
  carries on where it stopped.
- **"The VM didn't come up; starting it in safe mode".** The guest agent
  didn't answer within 90 s, so berth started again with home on Apple's
  share, mounted directly from the kernel command line. In safe mode the
  agent puts the guest's mount setup back to plain virtiofs. The next normal
  start uses berth's share again, but if the mount helper was involved, on
  the built-in driver until a berth update changes the helper.
  `console.log` shows what the failed boot was doing.
- **Still failing.** Rule out berth's file sharing:
  `BERTH_FILE_SHARING=apple berth start`. If that works, try
  `BERTH_FS_POLL_DRIVER=0 berth start` (berth's share without the polling
  driver), and report what you see.
- **"berth is starting or running".** Another `berth` holds the VM's lock
  (`~/.berth/run/`). Wait for it, or `berth stop`.

## Containers and ports

- **A published port isn't on the Mac.** `berth status` lists what's
  forwarded. If something on the Mac already uses the port, `daemon.log`
  says `cannot listen on …` and berth retries every 5 s until it's free.
- **A `--net host` server isn't reachable.** It appears on the Mac's
  `localhost` (not the LAN) within 2 s, if it listens on all interfaces or on
  loopback. Servers bound to one specific guest address aren't forwarded.
- **Ports below 1024 on loopback** need the route helper
  (`sudo berth helper install`): macOS lets users bind them only on all
  interfaces.
- **Container IPs (`curl 172.17.0.2`) don't answer.** They need the route
  helper. The `IPs:` line of `berth status` shows its state.
- **`<name>.berth.local` doesn't resolve.** macOS resolves `.local` names
  with multicast DNS; `berth domains` lists the names. Some VPN clients block
  multicast.
- **HTTPS warnings on `.berth.local`.** Run `berth tls trust` once.

## Importing

- **"OrbStack isn't running"**: `berth import` reads the other engine's
  Docker API, so start it first (`orb start`, or open Docker Desktop).
  Starting OrbStack points `/var/run/docker.sock` at it; the next
  `berth start` points it back.
- **A container was skipped.** Its settings may need something berth doesn't
  have (a device, a runtime); the reason is printed. Its image and volumes
  are still imported.
- **Images already in berth under the same tag aren't replaced.** Remove
  berth's copy first to take the other engine's.

## Machines

- **`ssh <machine>.berth` takes a while the first time.** berth installs
  sshd in the machine on first use (with the distro's package manager, so
  it needs the internet), and starts the machine if it's stopped.
- **`mac: /run/berth/mac.sock: ...`** in a machine made before berth had
  the `mac` command: restart the machine (`berth machine stop <name>`, then
  start) so it gets `/run/berth`.
- **No SSH agent in a machine.** berth uses the Mac's `SSH_AUTH_SOCK` (the
  one launchd sets for your login session); agents configured only with
  `IdentityAgent` in `~/.ssh/config` aren't picked up.

## Certificates

- **`x509: certificate signed by unknown authority` on a pull.** berth gives
  the VM the CA certificates macOS trusts, from the System and login
  keychains, at each start: trust the CA on the Mac (Keychain Access), then
  `berth stop --shutdown && berth start`. `daemon.log` says how many were
  passed. Containers don't get them; add them to your image.

## Files

- **`~/Berth` is empty.** It's only mounted while berth runs, with the IP route
  helper installed (`sudo berth helper install`; the VM accepts mounts only
  from root), and not with `"finder": false`. `daemon.log` says why if the
  mount failed.

- **A change on the Mac shows up late in a container.** Changes are
  forwarded from FSEvents, which takes ~0.6 s. Deleting a file on the Mac
  raises no IN_DELETE in containers, only an attribute event on its folder.
- **"Operation not permitted" changing ownership** (`chown`, `rsync -g`)
  on Apple's share: files there show as root-owned. Use berth's share
  (the default on macOS 27).

## Suspend and snapshots

- **`berth stop` shut down instead of suspending.** macOS saves a VM's
  state only while the Mac is unlocked; `daemon.log` says why.
- **The next start booted instead of resuming.** CPUs, memory or the file
  sharing setting changed since the suspend, or the state is from another
  macOS build. Containers were restarted.
- **A snapshot restore boots instead of resuming**, for the same reasons,
  or because the snapshot was taken while the Mac was locked (disk only).
- **Drop a suspended state** without resuming it: `berth start`, then
  `berth stop --shutdown`.

## Start over

- **Go back to a snapshot:** `berth snapshot restore <name>`.
- **Wipe the VM:** `berth delete --yes` removes containers, images, volumes
  and machines (`~/.berth/data`). Snapshots stay in `~/.berth/snapshots`.
  The next `berth start` provisions a new VM.

## Uninstall

```sh
berth stop --shutdown                # also unmounts ~/Berth
sudo berth helper uninstall          # the route helper and its routes, if installed
berth tls untrust                    # if you trusted the HTTPS CA
docker context use default && docker context rm berth
kubectl config delete-context berth  # if you enabled Kubernetes
podman system connection remove berth  # if you enabled Podman
```

The route helper also linked `/usr/local/bin/{docker,docker-compose,kubectl,podman,nerdctl}`
into `~/.berth/bin`, and `/var/run/docker.sock` to berth's socket. Remove
those that still point at berth:

```sh
ls -l /usr/local/bin/{docker,docker-compose,kubectl,podman,nerdctl,berth} /var/run/docker.sock
sudo rm /usr/local/bin/berth  # and each other link that points into ~/.berth or Berth.app
```

Remove the `Include ~/.berth/ssh/config` line from `~/.ssh/config`, then
delete `~/.berth` (the VM, snapshots and settings) and Berth.app.
