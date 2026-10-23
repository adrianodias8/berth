#!/bin/bash
# run.sh <label> [VAR=value...]: restart berth with berth's own share and the
# given settings, mount it, and run guest-bench.sh on it (and, with APPLE=1,
# on Apple's share first). Work files live in $BENCH (default ~/.berth-fsbench,
# which must be under your home so both shares see it).
#
#   bench/fs/make-tar.sh ~/.berth-fsbench
#   APPLE=1 bench/fs/run.sh default
#   bench/fs/run.sh poll BERTH_FS_POLL_DRIVER=1      # needs guest-kbuild.sh first
set -e
root="$(cd "$(dirname "$0")/../.." && pwd)"
B=$root/target/release/berth; fs=$root/bench/fs
BENCH=${BENCH:-$HOME/.berth-fsbench}
label=$1; shift
rel=${BENCH#"$HOME"/}
$B stop >/dev/null 2>&1 || true
env BERTH_EXPERIMENTAL_FS=1 "$@" $B start >/dev/null 2>&1
$B shell -- sudo $fs/guest-mount.sh >/dev/null
[ -n "${APPLE:-}" ] && echo "apple  $($B shell -- sudo $fs/guest-bench.sh $BENCH 2>/dev/null | grep RESULT)"
echo "$label  $($B shell -- sudo $fs/guest-bench.sh /mnt/berthfs/$rel 2>/dev/null | grep RESULT)"
