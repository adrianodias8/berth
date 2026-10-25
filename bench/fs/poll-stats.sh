#!/bin/sh
# Runs in the guest: the polling driver's counters (hits: reply arrived while
# polling; misses: fell back to the interrupt; skipped: background requests;
# shared: another CPU was polling; local_ioctls, noop_chmods: answered in the
# guest).
cd /sys/module/virtiofs_poll/parameters && for f in poll_us poll_hits poll_misses poll_skipped poll_shared poll_hit_ns local_ioctls noop_chmods; do printf "%s=%s " $f "$(cat $f)"; done; echo
