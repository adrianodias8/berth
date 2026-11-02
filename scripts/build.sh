#!/bin/sh
# Build the guest agent, then a signed release binary at target/release/berth.
#
# BERTH_VIRGL=auto (default) builds with 3D (the `virgl` feature) when
# virglrenderer is in the prefix (BERTH_VIRGL_PREFIX, else
# target/spikes/gpu/prefix, from bench/spikes/gpu/s2-virgl-angle.sh); 1
# requires it, 0 leaves it out.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
# ring (TLS) compiles C; use Xcode's toolchain even if the Command Line Tools
# are selected or broken.
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

prefix="${BERTH_VIRGL_PREFIX:-$root/target/spikes/gpu/prefix}"
features=""
case "${BERTH_VIRGL:-auto}" in
    0) ;;
    1)
        if [ ! -e "$prefix/lib/libvirglrenderer.dylib" ]; then
            echo "BERTH_VIRGL=1, but there's no virglrenderer in $prefix/lib; run bench/spikes/gpu/s2-virgl-angle.sh or set BERTH_VIRGL_PREFIX" >&2
            exit 1
        fi
        features="--features virgl"
        ;;
    auto) [ ! -e "$prefix/lib/libvirglrenderer.dylib" ] || features="--features virgl" ;;
    *) echo "BERTH_VIRGL is auto, 1 or 0, not ${BERTH_VIRGL}" >&2; exit 1 ;;
esac
echo "berth: virgl $([ -n "$features" ] && echo on || echo off)"

cargo build --release --manifest-path "$root/Cargo.toml" -p berth-agent --target aarch64-unknown-linux-musl
# The macOS guest agent, embedded for `berth mac` (optional without Swift).
if command -v swift >/dev/null 2>&1; then
    swift build -c release --package-path "$root/guest/mac-agent"
else
    echo "note: no swift; berth mac won't be able to install berth-gui-mac" >&2
fi
# build.rs finds the same prefix (BERTH_VIRGL_PREFIX or its default).
cargo build --release --manifest-path "$root/Cargo.toml" -p berth $features
BERTH_SIGN_ONLY=1 "$root/scripts/sign.sh" "$root/target/release/berth"
echo "built $root/target/release/berth"
