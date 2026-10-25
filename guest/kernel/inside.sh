#!/bin/bash
# Runs in the build container (build.sh). KERNEL_VERSION is a kernel.org
# release (default 7.2.12), its checksum checked, or "debian" for Debian's
# 6.12 kernel source. Either way Debian's 6.12 arm64 config is the start: the
# modules the VM uses built in (modules.txt), everything else that only real
# hardware needs left out, then config.fragment on top.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq linux-source-6.12 linux-config-6.12 build-essential bc bison flex \
    libssl-dev libelf-dev dwarves python3 kmod cpio rsync curl xz-utils patch gnupg >/dev/null

if [ "${KERNEL_VERSION:-7.2.12}" = debian ]; then
    version=$(dpkg-query -W -f '${Version}' linux-source-6.12)
    tree=/build/linux-$version
    if [ ! -d "$tree" ]; then
        mkdir -p "$tree"
        tar -xf /usr/src/linux-source-6.12.tar.xz -C "$tree" --strip-components=1
    fi
else
    version=${KERNEL_VERSION:-7.2.12}
    tree=/build/linux-$version
    if [ ! -d "$tree" ]; then
        base=https://cdn.kernel.org/pub/linux/kernel/v${version%%.*}.x
        tarball=/build/linux-$version.tar.xz
        [ -f "$tarball" ] || curl -fsSL -o "$tarball" "$base/linux-$version.tar.xz"
        # The checksums are signed by kernel.org's checksum autosigner, whose
        # key is pinned in guest/kernel (fingerprint checked below). Only the
        # signed text is used: anything outside the signature is ignored.
        key=/guest/kernel/kernel.org-autosigner.asc
        fpr=B8868C80BA62A1FFFAF5FDA9632D3A06589DA6B1
        gpg --batch --show-keys --with-colons "$key" | grep -qx "fpr:::::::::$fpr:" ||
            { echo "$key isn't kernel.org's checksum autosigner key" >&2; exit 1; }
        gpg --batch --yes --dearmor -o /tmp/autosigner.gpg "$key"
        curl -fsSL -o /tmp/sha256sums.asc "$base/sha256sums.asc"
        gpgv --keyring /tmp/autosigner.gpg --output /tmp/sha256sums /tmp/sha256sums.asc 2>/dev/null ||
            { echo "$base/sha256sums.asc: bad or missing signature" >&2; exit 1; }
        want=$(awk -v f="linux-$version.tar.xz" '$2 == f { print $1 }' /tmp/sha256sums)
        [ -n "$want" ] || { echo "no signed checksum for linux-$version.tar.xz" >&2; exit 1; }
        echo "$want  $tarball" | sha256sum -c - >/dev/null || { rm -f "$tarball"; exit 1; }
        mkdir -p "$tree"
        tar -xf "$tarball" -C "$tree" --strip-components=1
    fi
fi
cd "$tree"

# The polling virtio-fs driver (guest/kmod/virtiofs_poll), built in next to
# the stock one: every function in it is static, and it registers its own
# device ID and filesystem type. For 6.12 it's the driver's own source; for
# other series, berth's changes as a patch on that series' virtio_fs.c.
series=$(make -s kernelversion | cut -d. -f1,2)
if [ "$series" = 6.12 ]; then
    cp /guest/kmod/virtiofs_poll/virtiofs_poll.c fs/fuse/virtiofs_poll.c
    if ! diff -q /guest/kmod/virtiofs_poll/fuse_i.h fs/fuse/fuse_i.h >/dev/null; then
        echo "warning: guest/kmod/virtiofs_poll/fuse_i.h differs from this tree's fs/fuse/fuse_i.h" >&2
    fi
else
    patchfile=/guest/kernel/virtiofs_poll-$series.patch
    [ -f "$patchfile" ] || { echo "no polling driver port for $series ($patchfile)" >&2; exit 1; }
    cp fs/fuse/virtio_fs.c fs/fuse/virtiofs_poll.c
    patch -s fs/fuse/virtiofs_poll.c < "$patchfile"
fi
grep -q virtiofs_poll fs/fuse/Makefile || echo 'obj-$(CONFIG_VIRTIO_FS) += virtiofs_poll.o' >> fs/fuse/Makefile

xzcat /usr/src/linux-config-6.12/config.arm64_none_arm64.xz > .config
# Modules the VM loads (collected with Docker, k3s, Podman, nerdctl, IPv6
# and machines in use) become built in; every other module is dropped.
make -s olddefconfig
make -s LSMOD=/guest/kernel/modules.txt localyesconfig </dev/null >/dev/null 2>&1 || \
    make -s LSMOD=/guest/kernel/modules.txt localyesconfig </dev/null
scripts/kconfig/merge_config.sh -m .config /guest/kernel/config.fragment >/dev/null
# SoC platforms (and the drivers built in for them): a VM has none.
for platform in $(grep -oE '^config ARCH_[A-Z0-9_]+' arch/arm64/Kconfig.platforms | cut -d' ' -f2); do
    scripts/config --disable "$platform"
done
make -s olddefconfig

make -s -j"$(nproc)" Image modules
rm -rf /tmp/mods
make -s modules_install INSTALL_MOD_PATH=/tmp/mods >/dev/null
release=$(make -s kernelrelease)

cp arch/arm64/boot/Image /out/Image
cp .config /out/config
echo "$release" > /out/release
tar -C /tmp/mods -cf /out/modules.tar lib/modules
