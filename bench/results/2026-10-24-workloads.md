# Everyday workloads: OrbStack vs berth

2026-10-24, MacBook Pro M2 Max, macOS 27.0, OrbStack 2.2.5. `bench/workloads.sh
<context>`, each engine with the other idle; internet rows depend on the
connection (runs back to back).

| Workload | OrbStack | berth before | berth after vsock buffers |
|---|---|---|---|
| container run (mean of 10) | 0.27 s | 0.31 s | 0.32 s |
| Mac -> container, 2 GiB HTTP via host.docker.internal | **30.2 Gbit/s** | 3.4 Gbit/s | 6.9 Gbit/s |
| container -> Mac, 1 GiB HTTP POST | 17.8 Gbit/s | 18.6 Gbit/s | **29.6 Gbit/s** |
| internet download, 100 MB | 126 Mbit/s | 101 Mbit/s | – |
| DNS lookup, mean of 200 | 1.10 ms | **0.65 ms** | – |
| pull node:22-alpine | 3.76 s | 3.85 s | – |
| docker build, 5k-file context, no cache | **2.88 s** | 3.77 s | 3.79 s |
| volume write 1 GiB (fsync) | 1463 MB/s | 1506 MB/s | – |
| volume read 1 GiB (cold) | 6024 MB/s | 6024 MB/s | – |
| volume, 20k small files | 0.18 s | 0.23 s | – |

- host.docker.internal goes through vsock. macOS gives the Unix socket a VZ
  vsock connection is made of 8 KB buffers, and Linux vsock's receive
  buffer (which bounds the sender's credit) is 256 KB: raising both to
  4 MB doubled Mac -> container and made container -> Mac faster than
  OrbStack. Mac -> VM over vsock still tops out at ~7 Gbit/s; over the VM's
  NAT network it's ~10.5 Gbit/s, so OrbStack's 30 comes from its own
  network stack.
- docker build: the 10 MB context travels over the Docker socket bridge
  (vsock, Mac -> VM: the slow direction).
