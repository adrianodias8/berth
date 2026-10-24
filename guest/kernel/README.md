# berth's guest kernel

The VM boots this kernel by default when the app (or a development build)
ships one; otherwise, or with `berth config set kernel debian`, it boots the
VM's own Debian kernel as before.

It is Debian's 6.12 source and arm64 config, the same version the VM's Debian
kernel is, with:

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

Measured on an M2 Max (2026-10-24): the kernel boots in 0.31 s instead of
0.59 s, the whole boot in 2.3 s instead of 3.1 s. Workloads run the same
(DDEV Drupal CMS, idle CPU 1.38 % vs 1.37 %): the gains are boot time, no
runtime driver build, and the polling driver staying in use after a
snapshot restore.

## Building

```sh
guest/kernel/build.sh        # ~4 min on berth itself; output in target/kernel
./scripts/build-app.sh       # bundles target/kernel into Berth.app
berth restart
guest/kernel/check.sh        # what the kernel must support; KCHECK_K8S=1 adds k3s
```

The build runs in a Debian container on the current Docker context, which
must be arm64. The tree is kept in the `berth-kernel-build` volume.

## Safety net

The host counts starts on berth's kernel that didn't come up
(`~/.berth/guest/boot/berth-trying`, cleared by the agent once it's up).
After two in a row it writes that release to `berth-failed` and boots the
Debian kernel until a different release ships. Delete `berth-failed` to try
again.

## Updating

Debian ships security fixes for 6.12 as new `linux-source-6.12` packages:
rebuild and release. A newer upstream kernel (7.x) needs the polling driver
ported to its FUSE internals (`fuse_i.h`); `inside.sh` warns when the
driver's copy of that header differs from the tree's.
