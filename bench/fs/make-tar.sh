#!/bin/sh
# make-tar.sh <dir>: the benchmark's 20,000 x 1 KB files (100 directories),
# as <dir>/small.tar.
set -e
d=$1; mkdir -p "$d/src"
python3 - "$d/src" <<'PY'
import os, sys
for i in range(20000):
    sub = os.path.join(sys.argv[1], "d%03d" % (i // 200))
    os.makedirs(sub, exist_ok=True)
    with open(os.path.join(sub, "f%05d" % i), "wb") as f:
        f.write(os.urandom(1024))
PY
COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata -cf "$d/small.tar" -C "$d/src" .
rm -rf "$d/src"
