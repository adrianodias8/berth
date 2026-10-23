#!/bin/sh
# Runs in the guest: the polling driver's counters (hits: reply arrived while
# polling; misses: fell back to the interrupt; skipped: background requests).
cd /sys/module/virtiofs_poll/parameters && for f in poll_us poll_hits poll_misses poll_skipped poll_hit_ns; do printf "%s=%s " $f "$(cat $f)"; done; echo
