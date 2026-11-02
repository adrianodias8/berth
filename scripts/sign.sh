#!/bin/sh
# Ad-hoc sign a binary with the virtualization entitlement, then run it.
# Used as the cargo runner so `cargo run` works, and by build.sh.
# No hardened runtime: with `--options runtime`, a virgl build couldn't
# load its ad-hoc signed virglrenderer and ANGLE dylibs without the
# disable-library-validation entitlement or signing them with the same team.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$1"
shift
codesign --force --sign - --entitlements "$root/berth.entitlements" "$bin" 2>/dev/null
if [ "${BERTH_SIGN_ONLY:-}" = "1" ]; then exit 0; fi
exec "$bin" "$@"
