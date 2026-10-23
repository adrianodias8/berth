#!/bin/bash
# Runs in the guest (as root): guest-bench.sh <dir>. <dir> must hold small.tar
# (from make-tar.sh). One process extracts, reads (cold cache) and deletes
# 20k x 1 KB files; then 4 processes do the same in parallel. Prints one line.
TIMEFORMAT=%R
d=$1; rm -rf $d/w*; mkdir -p $d/w
e=$( { time tar -xf $d/small.tar -C $d/w 2>/dev/null; } 2>&1 )
sync; echo 3 > /proc/sys/vm/drop_caches
r=$( { time { (find $d/w -type f -print0 | xargs -0 cat > /dev/null) 2>/dev/null; }; } 2>&1 )
x=$( { time rm -rf $d/w 2>/dev/null; } 2>&1 )
mkdir -p $d/w1 $d/w2 $d/w3 $d/w4
pe=$( { time { (for i in 1 2 3 4; do tar -xf $d/small.tar -C $d/w$i & done; wait) 2>/dev/null; }; } 2>&1 )
sync; echo 3 > /proc/sys/vm/drop_caches
pr=$( { time { (for i in 1 2 3 4; do find $d/w$i -type f -print0 | xargs -0 cat > /dev/null & done; wait) 2>/dev/null; }; } 2>&1 )
px=$( { time { (for i in 1 2 3 4; do rm -rf $d/w$i & done; wait) 2>/dev/null; }; } 2>&1 )
echo "RESULT extract $e  read $r  delete $x  |  4x extract $pe  4x read $pr  4x delete $px"
