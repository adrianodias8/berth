#!/bin/sh
# Runs in the guest (as root): mount berth's share at /mnt/berthfs, through
# the polling driver when berth presents its device for it
# (BERTH_FS_POLL_DRIVER=1, and the module was built with guest-kbuild.sh),
# else through the built-in virtiofs driver. Optionally bind one subfolder of
# it over the same path in your home (e.g. guest-mount.sh ddev-bench), so
# containers use berth's share there.
[ -f /root/kmod/virtiofs_poll.ko ] && ! lsmod | grep -q virtiofs_poll && insmod /root/kmod/virtiofs_poll.ko
if ls /sys/bus/virtio/drivers/virtiofs_poll 2>/dev/null | grep -q '^virtio'; then
  type=virtiofs_poll
else
  type=virtiofs
fi
mkdir -p /mnt/berthfs
mountpoint -q /mnt/berthfs || mount -t $type berthfs /mnt/berthfs
if [ -n "$1" ]; then
  home=$(getent passwd 501 | cut -d: -f6)
  mkdir -p "/mnt/berthfs/$1"
  mountpoint -q "$home/$1" || mount --bind "/mnt/berthfs/$1" "$home/$1"
fi
mount | grep berthfs
