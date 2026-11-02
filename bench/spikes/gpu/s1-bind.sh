#!/bin/bash
# Spike S1: berth's custom virtio-gpu device binds the guest's stock
# virtio_gpu driver, and what the guest draws reaches the Mac.
#
#   bench/spikes/gpu/s1-bind.sh          check everything (berth running with "gui": true)
#   bench/spikes/gpu/s1-bind.sh kernel   only check the built kernel's config
#
# It never starts, stops or restarts berth: README.md says when to. Exits
# non-zero if any check fails; each line says PASS or FAIL.
set -u
root="$(cd "$(dirname "$0")/../../.." && pwd)"
BERTH="${BERTH:-$root/target/release/berth}"
gpu="$HOME/.berth/gpu"
log="$HOME/.berth/log/daemon.log"
failed=0
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; failed=1; }
check() { if eval "$2" >/dev/null 2>&1; then pass "$1"; else fail "$1"; fi; }

# The kernel: everything the display needs is built in. Dropped options
# (a dependency not met) vanish silently from the merged config.
config="${KERNEL_CONFIG:-$root/target/kernel/config}"
if [ -f "$config" ]; then
    for opt in DRM DRM_VIRTIO_GPU DRM_VIRTIO_GPU_KMS DRM_FBDEV_EMULATION FRAMEBUFFER_CONSOLE \
        SYNC_FILE DMA_SHARED_BUFFER UDMABUF DMABUF_HEAPS DMABUF_HEAPS_SYSTEM INPUT_EVDEV INPUT_UINPUT SND_VIRTIO; do
        check "kernel CONFIG_$opt=y" "grep -qx 'CONFIG_$opt=y' '$config'"
    done
else
    echo "SKIP  kernel config: no $config (build it with guest/kernel/build.sh)"
fi
[ "${1:-}" = kernel ] && exit $failed

# The host side: the flag, and the device in this start's log.
check 'config.json has "gui": true' "python3 -I -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get(\"gui\") is True else 1)' '$HOME/.berth/config.json'"
start=$(grep -n 'gpu: virtio-gpu' "$log" 2>/dev/null | tail -1 | cut -d: -f1)
if [ -z "$start" ]; then
    fail "daemon.log: the device was configured (\"gpu: virtio-gpu ...\"); restart berth after setting the flag"
    exit 1
fi
since() { tail -n +"$start" "$log"; }
pass "daemon.log: $(since | grep -m1 'gpu: virtio-gpu' | sed 's/.*gpu: //')"
check "daemon.log: the framework created the device" "since | grep -q 'gpu: device created'"
check "daemon.log: the guest driver set DRIVER_OK with edid and resource-uuid" \
    "since | grep -q 'gpu: driver ready, features edid, resource-uuid'"
since | grep 'gpu: ' | grep -v 'virtio-gpu\|device created\|driver ready' | head -5 | sed 's/^/INFO  /'

# The guest side: the driver bound, the mode we offered, the nodes.
# The script goes in on stdin: `berth shell` joins its arguments into one
# command line, which would split a `sh -c "<script>"` into words.
guest() { printf '%s\n' "$1" | "$BERTH" shell -- sudo sh; }
check "guest: virtio_gpu bound a virtio device" \
    "guest 'for d in /sys/bus/virtio/devices/*; do [ \"\$(cat \$d/device)\" = 0x0010 ] && readlink \$d/driver; done | grep -q virtio_gpu'"
check "guest: /dev/dri/card0 exists" "guest 'test -c /dev/dri/card0'"
mode=$(guest 'cat /sys/class/drm/card0-*/modes 2>/dev/null | head -1' | tr -d '\r')
want=$(python3 -I -c 'import json,sys
try: print(json.load(open(sys.argv[1])).get("gpu_display") or "1920x1080")
except Exception: print("1920x1080")' "$HOME/.berth/config.json")
check "guest: first mode is $want (from EDID; got '${mode:-none}')" "[ '$mode' = '$want' ]"
features=$(guest 'dmesg | grep -m1 "\[drm\] features"' | tr -d '\r')
echo "INFO  guest: ${features:-no [drm] features line in dmesg}"
check "guest: driver sees edid" "echo '$features' | grep -q '+edid'"
# S4's first half: the shared memory region shows up as host_visible.
if echo "$features" | grep -q '+host_visible'; then pass "guest: host-visible region seen (S4, first half)"; else echo "INFO  guest: no +host_visible (see S4)"; fi

# Frames: the device writes scanout 0 to a PNG while the marker exists.
mkdir -p "$gpu"
touch "$gpu/dump-frames"
rm -f "$gpu/scanout-0.png"
guest 'echo "berth-gpu S1 $(date)" > /dev/tty1; dd if=/dev/urandom of=/dev/fb0 bs=1M count=32 2>/dev/null; true' >/dev/null
for _ in $(seq 1 10); do [ -f "$gpu/scanout-0.png" ] && break; sleep 0.5; done
sleep 1.5
check "host: scanout 0 was written to $gpu/scanout-0.png" "test -s '$gpu/scanout-0.png'"
if [ -s "$gpu/scanout-0.png" ]; then
    size=$(sips -g pixelWidth -g pixelHeight "$gpu/scanout-0.png" 2>/dev/null | awk '/pixelWidth/ {w=$2} /pixelHeight/ {h=$2} END {print w "x" h}')
    check "host: the PNG is $want (got $size)" "[ '$size' = '$want' ]"
    # Random bytes written to fb0 make most pixels non-black.
    lit=$(python3 -I -c 'import sys,zlib,struct
d=open(sys.argv[1],"rb").read();p=8;z=b""
while p<len(d):
    n=struct.unpack(">I",d[p:p+4])[0]
    if d[p+4:p+8]==b"IDAT": z+=d[p+8:p+8+n]
    p+=12+n
r=zlib.decompress(z);w=struct.unpack(">I",d[16:20])[0];row=w*4+1
px=[r[i:i+row][1:] for i in range(0,len(r),row)]
lit=sum(1 for line in px for j in range(0,len(line),4) if line[j:j+3]!=b"\0\0\0")
print(lit*100//max(1,sum(len(l)//4 for l in px)))' "$gpu/scanout-0.png")
    check "host: what the guest drew is there ($lit% of pixels lit, want > 50)" "[ '${lit:-0}' -gt 50 ]"
fi
rm -f "$gpu/dump-frames"
exit $failed
