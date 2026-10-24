#!/bin/bash
# Build berth's guest kernel (guest/kernel/README.md) in a Debian container
# on the current Docker context, which must be arm64 (berth's is). The tree
# lives in a Docker volume (fast, and kept between builds); the results go
# to target/kernel:
#
#   Image          uncompressed arm64 kernel, booted directly (vm.rs)
#   release        its `uname -r`
#   modules.tar    /lib/modules/<release> metadata for modprobe (everything
#                  the VM needs is built in), installed by the agent
#   config         the final .config
set -euo pipefail
cd "$(dirname "$0")/../.."
out=$PWD/target/kernel
mkdir -p "$out"
docker volume create berth-kernel-build >/dev/null
docker run --rm \
    -v berth-kernel-build:/build \
    -v "$PWD/guest:/guest:ro" \
    -v "$out:/out" \
    debian:trixie bash /guest/kernel/inside.sh
echo "built $(cat "$out/release"): $out/Image ($(du -h "$out/Image" | cut -f1))"
