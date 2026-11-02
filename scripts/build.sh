#!/bin/sh
# Build the guest agent, then a signed release binary at target/release/berth.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
# ring (TLS) compiles C; use Xcode's toolchain even if the Command Line Tools
# are selected or broken.
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
cargo build --release --manifest-path "$root/Cargo.toml" -p berth-agent --target aarch64-unknown-linux-musl
# The macOS guest agent, embedded for `berth mac` (optional without Swift).
if command -v swift >/dev/null 2>&1; then
    swift build -c release --package-path "$root/guest/mac-agent"
else
    echo "note: no swift; berth mac won't be able to install berth-gui-mac" >&2
fi
cargo build --release --manifest-path "$root/Cargo.toml" -p berth
BERTH_SIGN_ONLY=1 "$root/scripts/sign.sh" "$root/target/release/berth"
echo "built $root/target/release/berth"
