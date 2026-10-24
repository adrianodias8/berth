#!/bin/bash
# berth debug <container> [command...]: a root shell inside a container's
# network, PID, UTS, IPC and cgroup namespaces, with the VM's tools (bash,
# ps, ss, curl, apt...) even when the image has none: distroless, scratch.
#
# The shell's root is a throwaway overlay of the VM's own root, so tools
# installed during the session (apt install strace) vanish when it ends,
# and nothing is added to the container or the VM. The container's files
# are at /container (its PID 1's root). The agent installs this file as
# /usr/local/libexec/berth-debug (agent/src/main.rs); `berth debug` runs it
# over SSH.
set -euo pipefail

if [ "${1:-}" = --inner ]; then
    # In the container's namespaces now, and a private mount namespace:
    # nothing mounted here is seen outside this session.
    name=$2 pid=$3
    shift 3
    mount -t tmpfs -o mode=0755 berth-debug /mnt
    mkdir /mnt/upper /mnt/work /mnt/root
    mount -t overlay overlay -o lowerdir=/,upperdir=/mnt/upper,workdir=/mnt/work /mnt/root
    r=/mnt/root
    # Still the VM's /proc here, where $pid is the container's PID 1. Its
    # name resolution, so apt and curl work as from the container. As plain
    # files: the VM's are symlinks into /run, which is replaced below.
    for f in resolv.conf hosts; do
        src=/proc/$pid/root/etc/$f
        [ -s "$src" ] || src=/etc/$f
        rm -f "$r/etc/$f"
        cat "$src" > "$r/etc/$f" 2>/dev/null || true
    done
    mount -t proc proc "$r/proc"   # the container's processes
    mount --rbind /dev "$r/dev"
    mount --rbind /sys "$r/sys"
    mount -t tmpfs tmpfs "$r/tmp"
    mount -t tmpfs tmpfs "$r/run"
    ln -sfn /proc/1/root "$r/container"
    export HOME=/root PS1="debug:$name \\w# "
    [ $# -gt 0 ] || set -- bash --norc
    exec chroot "$r" /bin/sh -c 'cd /container 2>/dev/null || cd /; exec "$@"' berth-debug "$@"
fi

name=${1:?usage: berth-debug <container> [command...]}
shift
pid=
for engine in docker podman nerdctl; do
    command -v "$engine" >/dev/null || continue
    pid=$("$engine" inspect -f '{{.State.Pid}}' "$name" 2>/dev/null || true)
    [ -n "$pid" ] && [ "$pid" != 0 ] && break
    pid=
done
if [ -z "$pid" ]; then
    echo "berth debug: no running container named $name" >&2
    exit 1
fi
# nsenter forks into the PID namespace; unshare gives the mount namespace.
exec nsenter --target "$pid" --net --pid --uts --ipc --cgroup -- \
    unshare --mount --propagation private -- "$0" --inner "$name" "$pid" "$@"
