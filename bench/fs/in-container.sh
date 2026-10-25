#!/bin/bash
# compare.sh's bind-mount steps (extract, read, delete 20,000 x 1 KB
# files), timed inside the container so container start isn't counted.
#
#   bench/fs/in-container.sh [docker context]     default: berth
CTX=${1:-berth}; W=$HOME/berth-sfbench; mkdir -p $W
[ -f $W/small.tar ] || python3 - "$W/small.tar" <<'PY'
import io, sys, tarfile
with tarfile.open(sys.argv[1], "w") as t:
    for i in range(20000):
        data = (b"x" * 1023 + b"\n")
        info = tarfile.TarInfo(f"d{i // 500}/f{i}.txt"); info.size = len(data)
        t.addfile(info, io.BytesIO(data))
PY
rm -rf $W/x; mkdir -p $W/x
cat > $W/steps.sh <<'SH'
t() { s=$(cut -d' ' -f1 /proc/uptime); "$@"; e=$(cut -d' ' -f1 /proc/uptime); echo "$(awk -v a=$s -v b=$e 'BEGIN{printf "%.2f", b-a}') s"; }
case $1 in
  extract) cd /w/x && t tar -xf /w/small.tar ;;
  read) t sh -c 'find /w/x -type f | wc -l >/dev/null; find /w/x -type f -exec cat {} + >/dev/null' ;;
  delete) t sh -c 'rm -rf /w/x/d*' ;;
esac
SH
for step in extract read delete; do
  printf "%s: " $step; docker --context $CTX run --rm -v "$W:/w" alpine sh /w/steps.sh $step
done
