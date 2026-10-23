#!/bin/bash
# System-level memory cost of an engine, independent of which process macOS
# attributes it to: anonymous + wired + compressor pages from vm_stat, sampled
# with the engine stopped, idle, during a 4 GiB workload and after it exits.
#
#   bench/sysmem.sh berth|orbstack
set -u
e=$1
root="$(cd "$(dirname "$0")/.." && pwd)"
BERTH="${BERTH:-$root/target/release/berth}"
ORB="${ORB:-$HOME/.orbstack/bin/orb}"
ctx=$([ "$e" = berth ] && echo berth || echo orbstack)

used_mb() {
    vm_stat | awk '
        /page size of/ { ps = $8 }
        /Anonymous pages/ { a = $3 }
        /Pages wired down/ { w = $4 }
        /occupied by compressor/ { c = $5 }
        END { gsub("\\.", "", a); gsub("\\.", "", w); gsub("\\.", "", c); printf "%d", (a + w + c) * ps / 1048576 }'
}
sample() { local s=0; for _ in 1 2 3; do s=$((s + $(used_mb))); sleep 1; done; echo $((s / 3)); }
stop() { "$BERTH" stop >/dev/null 2>&1; "$ORB" stop >/dev/null 2>&1; }

stop; sleep 10
base=$(sample)
if [ "$e" = berth ]; then "$BERTH" start >/dev/null; else "$ORB" start >/dev/null 2>&1; fi
until docker --context "$ctx" version >/dev/null 2>&1; do sleep 0.5; done
docker --context "$ctx" pull -q --platform linux/arm64 alpine >/dev/null
sleep 60
idle=$(sample)
docker --context "$ctx" run -d --name sysmem alpine sh -c 'head -c 4294967296 /dev/zero | tail; sleep 5' >/dev/null
peak=0
while [ "$(docker --context "$ctx" inspect -f '{{.State.Running}}' sysmem 2>/dev/null)" = true ]; do
    u=$(used_mb); [ "$u" -gt "$peak" ] && peak=$u; sleep 1
done
docker --context "$ctx" rm -f sysmem >/dev/null
sleep 60
after=$(sample)
echo "$e: stopped=${base} MB  idle=+$((idle - base)) MB  peak=+$((peak - base)) MB  60s-after-workload=+$((after - base)) MB"
stop
