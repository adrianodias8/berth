#!/bin/bash
# What the VM's kernel has to support, checked on the running VM (after
# `berth restart` with the kernel under test). Prints one line per check and
# exits non-zero if any failed. Leaves nothing behind.
set -u
fails=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }
check() { local name=$1; shift; if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi; }
vm() { berth shell -- "$@"; }

echo "kernel: $(vm uname -r 2>/dev/null)"
check "docker runs a container" docker run --rm alpine true
check "polling file driver mounted" vm 'grep -q virtiofs_poll /proc/mounts'
check "modprobe knows built-in modules" vm 'sudo modprobe -n overlay && sudo modprobe -n br_netfilter'
check "x86-64 container through Rosetta" docker run --rm --platform linux/amd64 alpine true
check "published port on the Mac" bash -c '
    docker run -d --name kcheck-web -p 18484:80 nginx:alpine && sleep 2
    curl -sf -o /dev/null http://localhost:18484'
check "container domain" curl -sf -o /dev/null -m 5 http://kcheck-web.berth.local
docker rm -f kcheck-web >/dev/null 2>&1
check "bind mount read and write" bash -c '
    d=$(mktemp -d "$HOME/.kcheck.XXXX") && echo hi > "$d/f" &&
    docker run --rm -v "$d:/w" alpine sh -c "cat /w/f && echo back > /w/g" && grep -q back "$d/g"; r=$?; rm -rf "$d"; exit $r'
check "inotify sees Mac edits" bash -c '
    d=$(mktemp -d "$HOME/.kcheck.XXXX") && touch "$d/f"
    docker run -d --name kcheck-watch -v "$d:/w" alpine sh -c "apk add -q inotify-tools && inotifywait -t 20 -e attrib,modify /w" >/dev/null
    sleep 6; echo x >> "$d/f"; docker wait kcheck-watch | grep -qx 0; r=$?
    docker rm -f kcheck-watch >/dev/null; rm -rf "$d"; exit $r'
check "IPv6 network" bash -c '
    docker network create --ipv6 --subnet fd00:c0de::/64 kcheck-v6 && docker run --rm --network kcheck-v6 alpine true; r=$?
    docker network rm kcheck-v6; exit $r'
check "host network" docker run --rm --network host alpine true
check "memory limit (cgroups)" bash -c '[ "$(docker run --rm --memory 64m alpine cat /sys/fs/cgroup/memory.max)" = 67108864 ]'
check "AppArmor profile on containers" bash -c 'docker run --rm alpine cat /proc/self/attr/current | grep -q docker-default'
check "iptables-restore (domains DNAT)" vm 'sudo iptables -t nat -S BERTH-SVC'
check "WireGuard and TUN" vm 'sudo ip link add kcheck-wg type wireguard && sudo ip link del kcheck-wg && test -c /dev/net/tun'
check "Podman container" vm 'sudo podman run --rm docker.io/library/alpine true'
check "nerdctl container" vm 'sudo nerdctl run --rm alpine true'
check "machine (systemd-nspawn)" bash -c 'berth machine create kcheck --distro debian:trixie && berth machine shell kcheck -- true; r=$?; berth machine delete kcheck --yes; exit $r'
check "network-isolated machine" bash -c 'berth machine create kchecki --distro debian:trixie --isolate-network; r=$?; berth machine delete kchecki --yes; exit $r'
if [ "${KCHECK_K8S:-}" = 1 ]; then
    check "Kubernetes (k3s) pod" bash -c 'berth k8s enable && kubectl --context berth run kcheck --image=alpine --restart=Never --command -- true &&
        kubectl --context berth wait --for=jsonpath={.status.phase}=Succeeded pod/kcheck --timeout=120s; r=$?
        kubectl --context berth delete pod kcheck --now; berth k8s disable; exit $r'
fi
echo "$fails failed"
exit $((fails > 0))
