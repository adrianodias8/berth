#!/bin/bash
# Runs in the build container (build.sh): Debian's kernel source and arm64
# config, the modules the VM uses built in (modules.txt), everything else
# that only real hardware needs left out, then config.fragment on top.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq linux-source-6.12 linux-config-6.12 build-essential bc bison flex \
    libssl-dev libelf-dev dwarves python3 kmod cpio rsync >/dev/null

src=$(ls /usr/src/linux-source-6.12.tar.xz)
version=$(dpkg-query -W -f '${Version}' linux-source-6.12)
tree=/build/linux-$version
if [ ! -d "$tree" ]; then
    rm -rf /build/linux-*
    mkdir -p "$tree"
    tar -xf "$src" -C "$tree" --strip-components=1
fi
cd "$tree"

# The polling virtio-fs driver (guest/kmod/virtiofs_poll), built in next to
# the stock one: every function in it is static, and it registers its own
# device ID and filesystem type.
cp /guest/kmod/virtiofs_poll/virtiofs_poll.c fs/fuse/virtiofs_poll.c
grep -q virtiofs_poll fs/fuse/Makefile || echo 'obj-$(CONFIG_VIRTIO_FS) += virtiofs_poll.o' >> fs/fuse/Makefile
if ! diff -q /guest/kmod/virtiofs_poll/fuse_i.h fs/fuse/fuse_i.h >/dev/null; then
    echo "warning: guest/kmod/virtiofs_poll/fuse_i.h differs from this tree's fs/fuse/fuse_i.h" >&2
fi

xzcat /usr/src/linux-config-6.12/config.arm64_none_arm64.xz > .config
# Modules the VM loads (collected with Docker, k3s, Podman, nerdctl, IPv6
# and machines in use) become built in; every other module is dropped.
make -s LSMOD=/guest/kernel/modules.txt localyesconfig </dev/null >/dev/null 2>&1 || \
    make -s LSMOD=/guest/kernel/modules.txt localyesconfig </dev/null
scripts/kconfig/merge_config.sh -m .config /guest/kernel/config.fragment >/dev/null
# SoC platforms (and the drivers built in for them): a VM has none.
for platform in $(grep -oE '^config ARCH_[A-Z0-9_]+' arch/arm64/Kconfig.platforms | cut -d' ' -f2); do
    scripts/config --disable "$platform"
done
make -s olddefconfig

make -s -j"$(nproc)" Image modules
make -s modules_install INSTALL_MOD_PATH=/tmp/mods >/dev/null
release=$(make -s kernelrelease)

cp arch/arm64/boot/Image /out/Image
cp .config /out/config
echo "$release" > /out/release
tar -C /tmp/mods -cf /out/modules.tar lib/modules
