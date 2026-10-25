# berth's guest kernel

The VM boots this kernel by default when the app (or a development build)
ships one; otherwise, or with `berth config set kernel debian`, it boots the
VM's own Debian kernel as before.

It is Linux 7.2.12 from kernel.org (checksum checked) on Debian's 6.12
arm64 config, with:

- **The modules the VM uses built in** (`modules.txt`, collected with Docker,
  k3s, Podman, nerdctl, IPv6 and machines in use), plus ones containers
  commonly need (`config.fragment`: WireGuard, IPVS, NFS and SMB volumes,
  more network types). No initrd, nothing loaded at runtime; `modprobe` still
  works from the module data the agent installs (`modules.tar`).
- **berth's polling virtio-fs driver built in** (guest/kmod/virtiofs_poll):
  no headers and compiler fetched with apt, no slower first start or offline
  fallback.
- **Hardware support compiled out:** SoC platforms, and with them the
  drivers Debian builds in for them.
- **Only the security modules the VM's userland uses** (AppArmor for
  Docker's profiles, landlock, yama, lockdown, integrity, BPF).
- **Boot options** (`src/vm.rs`): `workqueue.power_efficient=1` (queued
  kernel work runs on a CPU that's already awake) and
  `cgroup.memory=nokmem,nosocket` (no per-container accounting of kernel
  and socket memory). The agent adds zram swap the size of RAM (lz4),
  swappiness 20, huge pages on request only, and no disk scheduler.

Measured on an M2 Max: the kernel boots in 0.24 s instead of Debian's
0.59 s, the whole boot in ~2.3 s instead of 3.1 s, with no runtime driver
build, and the polling driver stays in use after a snapshot restore. 7.2
against the same build of 6.12 (2026-10-25,
bench/results/2026-10-23-ddev-drupal.md): DDEV's drush site:install
31.7–36.3 s vs 35.9–44.5 s, idle CPU the same (~1.4 %); 6.18.58 locked up
under k3s and was dropped.

## Building

```sh
guest/kernel/build.sh        # ~4 min on berth itself; output in target/kernel
                             # KERNEL_VERSION=7.2.13 (or debian, for 6.12):
                             # target/kernel-$KERNEL_VERSION
./scripts/build-app.sh       # bundles target/kernel into Berth.app
berth restart
guest/kernel/check.sh        # what the kernel must support; KCHECK_K8S=1 adds k3s
```

The build runs in a Debian container on the current Docker context, which
must be arm64. The tree is kept in the `berth-kernel-build` volume.

## Safety net

The host counts starts on berth's kernel that didn't come up
(`~/.berth/guest/boot/berth-trying`, cleared by the agent once it's up).
A stop asked for within 20 s of starting isn't counted. After two in a row
it writes that kernel (its release and its Image's size and time) to
`berth-failed` and boots the Debian kernel until a different kernel ships,
a rebuild included. Delete `berth-failed` to try again.

## Updating

A new 7.2.y stable release: change the default version in `inside.sh`,
rebuild, run `KCHECK_K8S=1 check.sh`, release. Another series needs the
polling driver ported: it is berth's changes as a patch on that series'
`fs/fuse/virtio_fs.c` (`virtiofs_poll-<series>.patch`); 6.12 uses the
driver's own source (guest/kmod/virtiofs_poll). 7.2 isn't a long-term
release: move to the next one before its support ends.
