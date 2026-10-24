#!/bin/bash
# run.sh <label> [VAR=value...]: restart berth with the given settings and run
# guest-bench.sh on the home share (berth's own by default; compare with
# BERTH_FILE_SHARING=apple). Work files live in $BENCH (default
# ~/.berth-fsbench, which must be under your home).
#
#   bench/fs/make-tar.sh ~/.berth-fsbench
#   bench/fs/run.sh berth
#   bench/fs/run.sh apple BERTH_FILE_SHARING=apple
set -e
root="$(cd "$(dirname "$0")/../.." && pwd)"
B=$root/target/release/berth; fs=$root/bench/fs
BENCH=${BENCH:-$HOME/.berth-fsbench}
label=$1; shift
$B stop >/dev/null 2>&1 || true
env "$@" $B start >/dev/null 2>&1
echo "$label ($($B status | sed -n 's/^Files: *//p'))  $($B shell -- sudo $fs/guest-bench.sh $BENCH 2>/dev/null | grep RESULT)"
