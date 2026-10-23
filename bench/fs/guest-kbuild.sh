#!/bin/sh
# Runs in the guest (as root): guest-kbuild.sh <repo>/guest/kmod/virtiofs_poll.
# Installs the running kernel's headers and a compiler if needed, builds the
# polling driver in /root/kmod, and prints warnings and errors.
set -e
[ -d /lib/modules/$(uname -r)/build ] || DEBIAN_FRONTEND=noninteractive apt-get install -y -q linux-headers-$(uname -r) gcc make >/dev/null
rm -rf /root/kmod && mkdir -p /root/kmod && cp "$1"/* /root/kmod/
cd /root/kmod && make 2>&1 | grep -E "error|warning|WARNING|undefined|LD \[M\]" || true
ls -la /root/kmod/virtiofs_poll.ko
