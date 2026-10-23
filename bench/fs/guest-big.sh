#!/bin/bash
# Runs in the guest (as root): guest-big.sh <dir>. Writes then reads (cold
# cache) a 2 GiB file.
d=$1; rm -f $d/big
w=$(dd if=/dev/zero of=$d/big bs=1M count=2048 conv=fsync 2>&1 | tail -1 | awk '{print $(NF-1),$NF}')
sync; echo 3 > /proc/sys/vm/drop_caches
r=$(dd if=$d/big of=/dev/null bs=1M 2>&1 | tail -1 | awk '{print $(NF-1),$NF}')
rm -f $d/big
echo "RESULT write $w  read $r"
