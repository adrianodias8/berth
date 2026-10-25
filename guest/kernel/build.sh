#!/bin/bash
# Build berth's guest kernel (guest/kernel/README.md) in a Debian container
# on the current Docker context, which must be arm64 (berth's is). The tree
# lives in a Docker volume (fast, and kept between builds); the results go
# to target/kernel (KERNEL_VERSION=7.2.13 builds that kernel.org release, or
# KERNEL_VERSION=debian Debian's 6.12, into target/kernel-$KERNEL_VERSION
# instead):
#
#   Image          uncompressed arm64 kernel, booted directly (vm.rs)
#   release        its `uname -r`
#   modules.tar    /lib/modules/<release> metadata for modprobe (everything
#                  the VM needs is built in), installed by the agent
#   config         the final .config
#   source.tar     with KERNEL_SOURCE=1: the kernel's exact source, as
#                  releases attach it (GPL-2.0)
set -euo pipefail
cd "$(dirname "$0")/../.."
out=$PWD/target/kernel${KERNEL_VERSION:+-$KERNEL_VERSION}
mkdir -p "$out"
docker volume create berth-kernel-build >/dev/null
docker run --rm \
    -v berth-kernel-build:/build \
    -v "$PWD/guest:/guest:ro" \
    -v "$out:/out" \
    -e KERNEL_VERSION="${KERNEL_VERSION:-}" \
    -e KERNEL_SOURCE="${KERNEL_SOURCE:-}" \
    debian:trixie bash /guest/kernel/inside.sh
echo "built $(cat "$out/release"): $out/Image ($(du -h "$out/Image" | cut -f1))"
