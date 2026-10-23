# berth

A free, open-source way to run Docker containers on Apple Silicon Macs, in the
spirit of OrbStack. Written in Rust on top of Apple's Virtualization.framework.

## Status

Milestones 1–2: a Linux VM with a Docker engine your existing `docker` CLI talks to,
with published ports and container domain names on the Mac.

- Debian 13 guest, booted through EFI, provisioned by cloud-init
- Docker API exposed on the Mac at `~/.berth/run/docker.sock` (bridged over vsock)
- Your home directory mounted in the VM at the same path (virtiofs), so
  `docker run -v "$PWD:/src" ...` works
- x86-64 images run through Rosetta
- Published ports (`-p 8080:80`) appear on the Mac, IPv4 and IPv6, following
  Docker's bind address: `-p 127.0.0.1:8080:80` stays local, the default listens
  on all interfaces. If a Mac process already holds `127.0.0.1:<port>`, it keeps
  local connections on that address; the container gets the rest. TCP only for now.
- Container domain names, no setup or root needed:
  - `<container>.berth.local`, and `<service>.<project>.berth.local` for Compose
  - any subdomain of those, e.g. `api.web.berth.local`
  - custom names from the `dev.berth.domains` label (comma-separated, `*.` wildcards,
    `.local` only); the `dev.orbstack.domains` label is read too
  - `berth.local` is the VM itself; `berth domains` lists everything
  
  Each running container gets its own address on the Mac↔VM network, so every
  port and protocol works without publishing (`curl http://web.berth.local`,
  `redis-cli -h cache.shop.berth.local`, `ping`).
- Memory balloon, so the VM's RAM size is a ceiling, not a reservation

Not yet: HTTPS and port-less HTTP for apps not on port 80, direct container IPs,
UDP published ports, `host.docker.internal`, Linux machines, GUI. See the roadmap below.

## Build

```sh
brew install rustup && rustup default stable
rustup target add aarch64-unknown-linux-musl   # for the guest agent
./scripts/build.sh        # builds the agent, then builds and ad-hoc signs target/release/berth
```

The binary needs the `com.apple.security.virtualization` entitlement, which the
script adds. `cargo run -- <args>` also signs before running.

## Use

```sh
berth start               # first start downloads Debian and installs Docker (a few minutes)
docker context use berth  # or: docker --context berth ps
berth status
berth domains             # container names and addresses
berth shell               # SSH into the VM; `berth shell uname -a` runs a command
berth stop
berth delete --yes        # wipe the VM disk (containers, images, volumes)
```

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

- `src/vm.rs`: VM configuration (EFI, virtio disk, NAT, virtiofs, Rosetta, vsock,
  balloon) and helpers that run calls on the main dispatch queue, where the VM lives
- `src/daemon.rs`: background process; control socket, socket bridges, shutdown
- `src/ports.rs`: published-port forwarding driven by Docker events
- `agent/`: the guest agent, a static Linux binary embedded in `berth` and
  refreshed into `~/.berth/guest/` on every start
  - `domains.rs`: Docker watcher, service IPs, guest addresses and NAT
  - `mdns.rs`: multicast DNS responder
- `src/provision.rs`: image download and checksum, disk clone, SSH key, cloud-init seed
- `guest/user-data.yaml`: guest provisioning (Docker, mounts, Rosetta binfmt, agent unit)

Logs are in `~/.berth/log/` (`daemon.log`, `console.log`).

## Roadmap

1. ✅ VM + Docker socket
2. ✅ Port forwarding for published ports (TCP), Rust guest agent, container
   domain names. Next: HTTPS with a local CA and port-less HTTP for any port,
   `host.docker.internal`, UDP published ports, direct container IPs (needs a root helper)
3. Custom kernel with direct boot for sub-second
   start; userspace networking and file sharing tuned for throughput
4. Linux machines (`berth create ubuntu`), menu-bar app, Kubernetes

## Compatibility notes

berth matches OrbStack's documented and externally observable behavior. It
contains no OrbStack code: it was written from OrbStack's public documentation and
by observing a running install from the outside (names it answers, addresses,
routes). OrbStack is a trademark of its owner; berth is not affiliated with it.
