#!/bin/sh
# Build the guest agent, then a signed release binary at target/release/berth.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$root/Cargo.toml" -p berth-agent --target aarch64-unknown-linux-musl
cargo build --release --manifest-path "$root/Cargo.toml" -p berth
BERTH_SIGN_ONLY=1 "$root/scripts/sign.sh" "$root/target/release/berth"
echo "built $root/target/release/berth"
