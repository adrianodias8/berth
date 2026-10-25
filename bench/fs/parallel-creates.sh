#!/bin/bash
# What unzip does per file (create, write, close, chmod, utime, stat), timed
# per operation in a container on a bind mount, from 1 and 8 threads: shows
# whether concurrent requests are served concurrently.
#
#   bench/fs/parallel-creates.sh [context]
set -euo pipefail
ctx=${1:-berth}
dir=$HOME/berth-opbench.noindex   # .noindex: Spotlight skips it
mkdir -p "$dir"
cp "$(dirname "$0")/ops.c" "$dir/"
docker --context "$ctx" run --rm -v "$dir:/w" alpine sh -c '
  apk add -q gcc musl-dev >/dev/null && gcc -O2 -static -o /tmp/ops /w/ops.c -lpthread &&
  for th in 1 8; do rm -rf /w/x; mkdir /w/x; /tmp/ops /w/x $th; done; rm -rf /w/x'
