# berth

A free, open-source way to run Docker containers on Apple Silicon Macs, in the
spirit of OrbStack. Written in Rust on top of Apple's Virtualization.framework.

## Features

**Engine**
- Debian 13 VM on Virtualization.framework, booted straight into its kernel (no
  EFI/GRUB once set up): Docker ready about 3 s after `berth start`
- Docker's own engine (docker-ce 29) with the Docker API at `~/.berth/run/docker.sock`,
  `/var/run/docker.sock` (via the helper) and a `berth` docker context made current on start
- Official `docker`, Compose, Buildx and `kubectl` CLIs in `~/.berth/bin`, linked
  wherever none work, so berth doesn't depend on another Docker app
- x86-64 images through Rosetta
- Memory balloon steered by guest usage and memory-pressure events: the VM can
  use what it needs but doesn't hoard. (Virtualization.framework doesn't hand
  already-used pages back to macOS; see "Limits".)
- ASIF disk image on macOS 26+: deleting images and containers frees disk space
  on the Mac. VM data is excluded from Time Machine.

**Networking**
- Published ports (`-p 8080:80`, TCP and UDP) on the Mac, IPv4 and IPv6, following
  Docker's bind address
- Container domains with no setup: `<container>.berth.local`,
  `<service>.<project>.berth.local` for Compose, any subdomain, plus custom names
  from the `dev.berth.domains` (or `dev.orbstack.domains`) label; `berth domains`
  lists them. Every port and protocol works on a domain, no publishing needed.
- `http://` and `https://<name>.berth.local` without port numbers: the web port is
  detected (override: `dev.berth.http-port`), HTTPS uses a local CA limited to
  `.local` names (`berth tls trust`, or the app's setup)
- Direct container IPs from the Mac (`curl 172.17.0.2`) for every bridge network
- `host.docker.internal` reaches the Mac, including localhost-only servers;
  containers use the Mac's DNS, so VPN and split DNS apply

**Development**
- Bind mounts of your home directory at the same path, with file change
  notifications forwarded so hot reload works (edits and new files; deletions are
  seen on the watcher's next rescan)
- Kubernetes: `berth k8s enable` gives a single-node k3s using berth's Docker, so
  locally built images run in pods without a registry; `berth` kubectl context,
  API on `localhost:6443`, pod and service IPs reachable from the Mac
- Linux machines: `berth machine create dev --distro ubuntu:noble` (Debian,
  Fedora, Alpine, Arch, Rocky, Alma, openSUSE…) with your user, sudo, your Mac
  home, systemd, and `dev.berth.local`; `berth machine shell dev`

**App**: Berth.app (SwiftUI) with first-run setup, a menu-bar item, containers,
images, volumes, networks, Kubernetes, machines, terminals, logs, files and stats.

**Limits** (measured against OrbStack 2.2.5 in `bench/results/`)
- Small-file I/O on bind mounts is ~3.5x slower: Apple's virtiofs costs ~450 µs
  per operation, and nothing on the guest side changes that.
- Memory used by a workload isn't returned to macOS after it ends (OrbStack in the
  same test didn't return it within 2 minutes either).
- Deleting a file on the Mac doesn't raise an inotify event in containers.

## Build

```sh
brew install rustup && rustup default stable
rustup target add aarch64-unknown-linux-musl   # for the guest agent
./scripts/build.sh        # builds the agent, then builds and ad-hoc signs target/release/berth
```

The binary needs the `com.apple.security.virtualization` entitlement, which the
script adds. `cargo run -- <args>` also signs before running.

The app (needs Xcode):

```sh
./scripts/build-app.sh    # builds everything into target/Berth.app
open target/Berth.app     # or copy it to /Applications
```

## The app

Berth.app embeds the `berth` CLI (`Contents/Helpers/berth`) and talks to the
daemon's control socket and Docker's API over unix sockets; it adds no services of
its own. Closing the window keeps it in the menu bar.

- **Menu bar**: status, start/stop, running containers with start/stop and open-in-browser
- **Containers**: grouped by Compose project; start/stop/restart/delete, open in
  browser, copy domain/IP. Per container:
  - **Info**: image, domain, IPs, published ports, labels
  - **Logs**: live, follow mode
  - **Terminal**: interactive shell over Docker's exec API (bash if present, else sh)
  - **Files**: browse, view and download files
  - **Stats**: live CPU and memory charts
- **Images** (with pull and progress), **Volumes**, **Networks** (subnets, container
  counts, IP routing status)
- **Machine**: a shell in the VM
- **Settings**: CPUs and memory (written to `~/.berth/config.json`, applied on restart),
  start on app launch, open at login, install the IP route helper, trust the HTTPS
  certificate

Launch arguments pick the starting screen, e.g.
`open target/Berth.app --args -openSection Images` or
`--args -openContainer web -openTab Logs`.

## Use

```sh
berth start               # first start downloads Debian and installs Docker (a few minutes)
docker context use berth  # or: docker --context berth ps
berth status
berth domains             # container names and addresses
berth tls trust           # trust the local CA for https://*.berth.local (one-time, asks for password)
berth k8s enable          # Kubernetes (k3s) with a `berth` kubectl context
berth machine create dev  # an Ubuntu machine; then: berth machine shell dev
berth shell               # SSH into the VM; `berth shell uname -a` runs a command
berth stop
berth delete --yes        # wipe the VM disk (containers, images, volumes)
```

### Direct container IPs (optional, one-time sudo)

Reaching container IPs needs a route on the Mac for each Docker subnet, and
routes need root. A small helper does only that:

```sh
sudo berth helper install     # run in a terminal; asks for your password
sudo berth helper uninstall   # removes it and its routes
```

It copies `berth` to a root-owned `/Library/PrivilegedHelperTools/dev.berth.helper`
and runs it as a LaunchDaemon on `/var/run/dev.berth.helper.sock`, which only your
user can use. It accepts only private subnets (10/8, 172.16/12, 192.168/16,
100.64/10, 198.18/15) pointing at a VM bridge interface, and refuses any subnet that
overlaps a network the Mac already routes. Routes follow Docker networks as they
are created and removed, and are withdrawn when berth stops. `berth status` shows
the state on the `IPs:` line; the helper logs to `/var/log/dev.berth.helper.log`.

berth keeps its own data in `~/.berth` and its own Docker context, so it runs
alongside OrbStack or Docker Desktop without touching them.

## How it works

```
docker CLI ── ~/.berth/run/docker.sock ─┐
ssh       ── ~/.berth/run/ssh.sock ────┼─ berth daemon ══ vsock:2400 ══ berth-agent ─┬─ /run/docker.sock
browser   ── localhost:<published> ────┘   (Rust, VZ)                   (Rust, guest) ├─ sshd
                                                                                      └─ 127.0.0.1:<published>
```

Every connection into the guest goes through one vsock port. The host sends a
one-line header (`unix /run/docker.sock` or `tcp 127.0.0.1:8080`) and the agent
connects there and splices bytes. The daemon follows Docker's event stream to
open and close host listeners as containers with published ports start and stop.

Domain names work differently, without the host daemon in the data path:

```
curl web.berth.local ── mDNS query on every interface ──> berth-agent answers 192.168.64.229
        └─ TCP to 192.168.64.229 ── VM NAT network ──> guest owns .229 ── DNAT ──> web (172.17.0.2)
```

macOS resolves every `.local` name with multicast DNS, including on the VM's
network interface, so the agent's responder needs no resolver file. The agent
assigns each container a service IP from the top quarter of the VM's NAT subnet
(the DHCP server allocates from the bottom), adds it to the guest's interface,
and DNATs it to the container with iptables. Addresses are a hash of the container
name, so they stay stable across restarts. When a container stops, the agent
removes the address and sends an mDNS goodbye so the Mac drops the cached name.

`host.docker.internal` runs the other way, guest to Mac:

```
container ── DNS 198.19.248.253 (agent) ── host.docker.internal = 198.19.248.254
container ── TCP 198.19.248.254:3000 ── iptables REDIRECT ── agent ══ vsock:2401 ══ berth daemon ── 127.0.0.1:3000 (or [::1])
```

The agent sets Docker's `dns` to its own server (keeping any other `daemon.json`
settings) and relays names it doesn't own to the guest's resolver, which forwards
to the Mac. The daemon only ever connects to the Mac's loopback, on the port the
container asked for.

Web access to domains goes through the agent and, for HTTPS, the host:

```
http://web.berth.local  ── service IP:80  ── REDIRECT ── agent: detect web port ── container:3000
https://web.berth.local ── service IP:443 ── REDIRECT ── agent ══ vsock ══ daemon: TLS ══ agent ── container:3000
```

The agent finds the web port from the container's listening sockets
(`/proc/<pid>/net/tcp*`, skipping loopback-only ones), probes them with an HTTP
request, and caches the answer until the container restarts. The daemon holds
berth's CA (`~/.berth/tls`) and issues a 30-day certificate per name on first use.
The CA carries an X.509 name constraint permitting only `.local`, and the daemon
refuses any other name, so trusting it cannot be abused for real sites.

- `src/vm.rs`: VM configuration (EFI, virtio disk, NAT, virtiofs, Rosetta, vsock,
  balloon) and helpers that run calls on the main dispatch queue, where the VM lives
- `src/daemon.rs`: background process; control socket, socket bridges, shutdown
- `src/ports.rs`: published-port forwarding driven by Docker events
- `src/routes.rs`, `src/helper.rs`: container subnet routes and the root helper
- `src/watch.rs`: FSEvents → inotify forwarding for bind mounts
- `src/memory.rs`: balloon controller
- `src/k8s.rs`, `src/machines.rs`: Kubernetes and Linux machines (agent: `k8s.rs`, `machines.rs`)
- `src/cli_tools.rs`: bundled Docker/Compose/Buildx/kubectl CLIs
- `agent/`: the guest agent, a static Linux binary embedded in `berth` and
  refreshed into `~/.berth/guest/` on every start
  - `domains.rs`: Docker watcher, service IPs, guest addresses and NAT
  - `mdns.rs`: multicast DNS responder
  - `dns.rs`: DNS server for containers
  - `hostnet.rs`: `host.docker.internal` forwarding to the Mac
  - `web.rs`: web port detection, HTTP/HTTPS entry for domains
- `src/tls.rs`: the local CA, per-name certificates, keychain trust
- `src/provision.rs`: image download and checksum, disk clone, SSH key, cloud-init seed
- `guest/user-data.yaml`: guest provisioning (Docker, mounts, Rosetta binfmt, agent unit)
- `app/`: the SwiftUI app (SwiftTerm for terminals, Swift Charts for stats)

Logs are in `~/.berth/log/` (`daemon.log`, `console.log`).

## Roadmap

Informed by OrbStack users' most-requested features (public issue tracker and
discussions) and the limits above:

- **Engine choice**: Podman and containerd/nerdctl alongside Docker (OrbStack's
  most-upvoted open request)
- **Faster file sharing**: a userspace file server (e.g. NFS) over the VM network
  to beat virtiofs on metadata-heavy workloads
- **Machines**: snapshots, cloud-init / custom images, GUI apps
- **Networking**: bridged mode (container IPs on the LAN) through the root helper,
  `.berth.local` names reachable from other devices, client source IP preservation
- **Memory**: returning freed memory to macOS needs a VMM with free-page reporting
  (Hypervisor.framework / libkrun); worth it only if it keeps Rosetta working
- **Distribution**: signed and notarized releases with auto-update; data location
  setting

## Compatibility notes

berth matches OrbStack's documented and externally observable behavior. It
contains no OrbStack code: it was written from OrbStack's public documentation and
by observing a running install from the outside (names it answers, addresses,
routes). Where OrbStack relies on a macOS entitlement Apple grants only to approved
developers (its per-network vmnet interfaces), berth uses a root route helper instead. OrbStack is a trademark of its owner; berth is not affiliated with it.
