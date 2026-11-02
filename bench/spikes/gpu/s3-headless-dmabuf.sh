#!/bin/bash
# Spike S3: a headless Wayland compositor renders on the GPU's render node
# and hands a frame out as a dma-buf, which is how GUI sessions are captured
# (docs/plans/gui-vms-agent-testing.md, "Sessions, not one global screen").
# Runs in a throwaway machine, gui-test-s3, deleted at the end.
#
#   bench/spikes/gpu/s3-headless-dmabuf.sh
#
# Needs S1 (the device bound) and, for GPU rendering, the 3D path (S2 and
# virgl routed in src/gpu); with 2D only it reports which half is missing.
set -u
root="$(cd "$(dirname "$0")/../../.." && pwd)"
BERTH="${BERTH:-$root/target/release/berth}"
m=gui-test-s3
failed=0
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; failed=1; }

if ! "$BERTH" shell -- test -c /dev/dri/renderD128; then
    fail "no /dev/dri/renderD128 in the VM: the driver exposes a render node only with virgl (S2)"
    exit 1
fi
"$BERTH" machine create "$m" --distro debian >/dev/null || { fail "creating $m"; exit 1; }
trap '"$BERTH" machine delete "$m" --yes >/dev/null 2>&1' EXIT
# Machines get the render node bound in with --gui (Phase 4); until then
# the check below says so and stops.
run() { "$BERTH" machine shell "$m" -- sudo sh -c "$1"; }
run 'apt-get update -qq && apt-get install -y -qq sway grim wayland-utils mesa-utils >/dev/null' || { fail "installing sway, grim"; exit 1; }
run 'test -c /dev/dri/renderD128' && pass "render node visible in the machine" || { fail "render node not bound into the machine (needs Bind=/dev/dri/renderD128)"; exit 1; }
run 'export XDG_RUNTIME_DIR=/run/s3; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
     WLR_BACKENDS=headless WLR_RENDERER=gles2 WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128 WLR_LIBINPUT_NO_DEVICES=1 \
       sway -c /dev/null >/run/s3/sway.log 2>&1 &
     for i in $(seq 1 20); do ls $XDG_RUNTIME_DIR/wayland-* >/dev/null 2>&1 && break; sleep 0.5; done
     export WAYLAND_DISPLAY=$(basename $(ls $XDG_RUNTIME_DIR/wayland-? | head -1))
     wayland-info | grep -E "zwp_linux_dmabuf_v1|ext_image_copy_capture|zwlr_screencopy" > /run/s3/protocols
     WAYLAND_DEBUG=1 grim /run/s3/shot.png 2>/run/s3/grim.trace; echo $? > /run/s3/grim.rc'
run 'grep -q zwp_linux_dmabuf_v1 /run/s3/protocols' && pass "compositor offers linux-dmabuf" || fail "no linux-dmabuf"
run 'grep -q -i "virgl\|virtio" /run/s3/sway.log || grep -q "GL_RENDERER" /run/s3/sway.log; grep -i "GL_RENDERER\|EGL" /run/s3/sway.log | head -3' | sed 's/^/INFO  /'
run 'grep -q -i "llvmpipe\|softpipe" /run/s3/sway.log' && fail "sway fell back to software rendering" || pass "no software renderer in sway's log"
run '[ "$(cat /run/s3/grim.rc)" = 0 ] && test -s /run/s3/shot.png' && pass "grim captured a frame" || fail "grim failed"
run 'grep -q "linux_dmabuf\|create_params" /run/s3/grim.trace' && pass "the capture used a dma-buf" || echo "INFO  grim used shared memory, not a dma-buf (see its trace)"
exit $failed
