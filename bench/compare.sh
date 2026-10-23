#!/bin/bash
# Side-by-side measurements of berth and OrbStack on this Mac.
#
#   bench/compare.sh [berth|orbstack]...      (default: both)
#
# Runs each engine alone (the other is stopped), with the same workloads:
# startup, idle CPU/memory, memory returned after a workload, bind-mount file
# I/O, and port-forward / domain throughput. Host memory is measured
# system-wide (anonymous + wired + compressed pages from vm_stat) relative to
# a baseline with both engines stopped: per-process `footprint` misses guest
# memory that a custom VM monitor (OrbStack's) keeps outside its process
# accounting. Other apps add some noise. Results go to stdout as Markdown and
# to bench/results/<date>.md.
set -u
root="$(cd "$(dirname "$0")/.." && pwd)"
BERTH="${BERTH:-$root/target/release/berth}"
ORB="${ORB:-$HOME/.orbstack/bin/orb}"
WORK="$HOME/.berth-bench"           # under $HOME so both engines share it
mkdir -p "$root/bench/results" "$WORK"
out="$root/bench/results/$(date +%Y-%m-%d-%H%M).md"
engines=("${@:-berth orbstack}")
engines=(${engines[@]})

now() { python3 -c 'import time; print(time.time())'; }
since() { python3 -c "import sys; print(f'{float(sys.argv[2]) - float(sys.argv[1]):.2f}')" "$1" "$(now)"; }
log() { echo "[$(date +%H:%M:%S)] $*" >&2; }
row() { echo "| $1 | $2 | $3 |" | tee -a "$out.$4"; }

ctx_of() { [ "$1" = berth ] && echo berth || echo orbstack; }
d() { docker --context "$CTX" "$@"; }

start_engine() { if [ "$1" = berth ]; then "$BERTH" start >/dev/null; else "$ORB" start >/dev/null 2>&1; fi; }
stop_engine() { if [ "$1" = berth ]; then "$BERTH" stop >/dev/null 2>&1; else "$ORB" stop >/dev/null 2>&1; fi; }

# VM-side processes: berth's daemon + the Virtualization.framework VM process;
# OrbStack's VM manager ("OrbStack Helper") + any VZ VM process it owns.
engine_pids() {
    if [ "$1" = berth ]; then
        pgrep -f "release/berth daemon|Contents/Helpers/berth daemon"
        pgrep -f "com.apple.Virtualization.VirtualMachine"
    else
        pgrep -f "OrbStack Helper"
        pgrep -f "com.apple.Virtualization.VirtualMachine"
    fi | sort -u
}

# System-wide memory in use, MB (averaged over 3 samples).
sys_mb() {
    local total=0
    for _ in 1 2 3; do
        total=$((total + $(vm_stat | awk '
            /page size of/ { ps = $8 }
            /Anonymous pages/ { a = $3 }
            /Pages wired down/ { w = $4 }
            /occupied by compressor/ { c = $5 }
            END { gsub("\\.", "", a); gsub("\\.", "", w); gsub("\\.", "", c); printf "%d", (a + w + c) * ps / 1048576 }')))
        sleep 1
    done
    echo $((total / 3))
}

# Engine memory: system-wide use above the stopped baseline.
mem_mb() { echo $(($(sys_mb) - BASE)); }

cpu_seconds() {
    local total=0
    for pid in $(engine_pids "$1"); do
        local t
        t=$(ps -o time= -p "$pid" 2>/dev/null | awk -F'[:.]' '{ if (NF==3) print $1*60+$2+$3/100; else print $1*3600+$2*60+$3+$4/100 }')
        total=$(python3 -c "print($total + ${t:-0})")
    done
    echo "$total"
}

wait_docker() {
    for _ in $(seq 1 600); do
        d version >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    return 1
}

prepare_files() {
    [ -f "$WORK/small.tar" ] && return
    log "creating 20,000-file tarball"
    python3 - "$WORK/small.tar" <<'EOF'
import io, sys, tarfile
with tarfile.open(sys.argv[1], "w") as t:
    for i in range(20000):
        data = (b"x" * 1023 + b"\n")
        info = tarfile.TarInfo(f"d{i // 500}/f{i}.txt"); info.size = len(data)
        t.addfile(info, io.BytesIO(data))
EOF
}

bench() {
    local e=$1
    CTX=$(ctx_of "$e")
    : > "$out.$e"
    log "== $e"
    for other in berth orbstack; do stop_engine "$other"; done
    sleep 10
    BASE=$(sys_mb)

    # Startup, from stopped to Docker answering.
    local starts=()
    for run in 1 2 3; do
        stop_engine "$e"; sleep 3
        local t0; t0=$(now)
        start_engine "$e"; wait_docker
        starts+=("$(since "$t0")")
        log "start $run: ${starts[${#starts[@]}-1]}s"
    done
    row "Startup, stopped → Docker ready (3 runs)" "${starts[*]} s" "" "$e"

    d pull -q alpine >/dev/null; d pull -q nginx:alpine >/dev/null
    local running; running=$(d ps -q | wc -l | tr -d ' ')
    row "Containers already running during idle test" "$running" "" "$e"

    # Idle.
    log "idle: settling 30s, measuring 60s"
    sleep 30
    local c0 c1; c0=$(cpu_seconds "$e"); sleep 60; c1=$(cpu_seconds "$e")
    row "Idle CPU (VM processes, avg over 60 s)" "$(python3 -c "print(f'{($c1-$c0)/60*100:.2f}')") %" "" "$e"
    row "Idle host memory (system-wide, over stopped)" "+$(mem_mb) MB" "" "$e"

    # Anonymous memory: fill 4 GiB, exit, watch it come back.
    log "memory: 4 GiB anonymous"
    d run -d --name bench-mem alpine sh -c 'head -c 4294967296 /dev/zero | tail; sleep 5' >/dev/null
    local peak=0
    for _ in $(seq 1 40); do
        local f; f=$(mem_mb); [ "$f" -gt "$peak" ] && peak=$f
        [ "$(d inspect -f '{{.State.Running}}' bench-mem 2>/dev/null)" = true ] || break
        sleep 1
    done
    d rm -f bench-mem >/dev/null 2>&1
    local a0; a0=$(mem_mb); sleep 30; local a30; a30=$(mem_mb); sleep 30
    local a60; a60=$(mem_mb); sleep 60; local a120; a120=$(mem_mb)
    row "4 GiB anonymous: peak / after exit / +30 s / +60 s / +120 s" "+$peak / +$a0 / +$a30 / +$a60 / +$a120 MB" "" "$e"

    # Page cache: write and read a 4 GiB file inside the container, delete it.
    log "memory: 4 GiB page cache"
    d run --rm alpine sh -c 'dd if=/dev/zero of=/tmp/f bs=1M count=4096 2>/dev/null; cat /tmp/f >/dev/null; rm /tmp/f' >/dev/null
    local p0; p0=$(mem_mb); sleep 60; local p60; p60=$(mem_mb); sleep 60; local p120; p120=$(mem_mb)
    row "4 GiB page cache: after / +60 s / +120 s" "+$p0 / +$p60 / +$p120 MB" "" "$e"

    # Bind-mount file I/O on a home directory path.
    prepare_files
    local m="$WORK/$e"; rm -rf "$m"; mkdir -p "$m"
    local t
    t=$(now); d run --rm -v "$WORK:/w" alpine true >/dev/null; local base; base=$(since "$t")
    row "Container run overhead (alpine true)" "$base s" "" "$e"
    t=$(now); d run --rm -v "$WORK:/w" alpine sh -c "cd /w/$e && tar -xf /w/small.tar" >/dev/null
    row "Bind mount: extract 20,000 × 1 KB files" "$(since "$t") s" "" "$e"
    t=$(now); d run --rm -v "$WORK:/w" alpine sh -c "find /w/$e -type f -exec cat {} + >/dev/null" >/dev/null
    row "Bind mount: read all 20,000 files" "$(since "$t") s" "" "$e"
    t=$(now); d run --rm -v "$WORK:/w" alpine sh -c "rm -rf /w/$e/d*" >/dev/null
    row "Bind mount: delete 20,000 files" "$(since "$t") s" "" "$e"
    t=$(now); d run --rm -v "$WORK:/w" alpine sh -c "dd if=/dev/zero of=/w/$e/big bs=1M count=1024 conv=fsync 2>/dev/null" >/dev/null
    row "Bind mount: write 1 GiB (fsync)" "$(since "$t") s" "" "$e"
    t=$(now); d run --rm -v "$WORK:/w" alpine sh -c "dd if=/w/$e/big of=/dev/null bs=1M 2>/dev/null" >/dev/null
    row "Bind mount: read 1 GiB" "$(since "$t") s" "" "$e"
    rm -rf "$m"

    # Network: Mac ← container, through the port forward and the domain.
    log "network"
    d rm -f bench-web >/dev/null 2>&1
    d run -d --name bench-web -p 18999:80 nginx:alpine >/dev/null
    d exec bench-web sh -c 'dd if=/dev/zero of=/usr/share/nginx/html/1g bs=1M count=1024 2>/dev/null'
    sleep 2
    local domain; [ "$e" = berth ] && domain=bench-web.berth.local || domain=bench-web.orb.local
    local s1 s2
    s1=$(curl -s -o /dev/null -w '%{speed_download}' http://localhost:18999/1g)
    s2=$(curl -s -o /dev/null -w '%{speed_download}' "http://$domain/1g")
    row "Download 1 GiB via localhost port forward" "$(python3 -c "print(f'{$s1*8/1e9:.2f}')") Gbit/s" "" "$e"
    row "Download 1 GiB via container domain" "$(python3 -c "print(f'{$s2*8/1e9:.2f}')") Gbit/s" "" "$e"
    d rm -f bench-web >/dev/null
}

for e in "${engines[@]}"; do bench "$e"; done

{
    echo "# berth vs OrbStack — $(date '+%Y-%m-%d %H:%M')"
    echo
    echo "$(sysctl -n machdep.cpu.brand_string), $(($(sysctl -n hw.memsize) >> 30)) GiB, macOS $(sw_vers -productVersion)."
    echo "berth $(git -C "$root" rev-parse --short HEAD); OrbStack $(defaults read /Applications/OrbStack.app/Contents/Info.plist CFBundleShortVersionString 2>/dev/null)."
    echo
    echo "| Test | berth | OrbStack |"
    echo "|---|---|---|"
    # A single-engine run leaves the other column empty.
    touch "$out.berth" "$out.orbstack"
    [ -s "$out.berth" ] || sed 's/|[^|]*|[^|]*|$/| | |/' "$out.orbstack" > "$out.berth"
    [ -s "$out.orbstack" ] || sed 's/|[^|]*|[^|]*|$/| | |/' "$out.berth" > "$out.orbstack"
    paste -d'\t' "$out.berth" "$out.orbstack" | while IFS=$'\t' read -r b o; do
        name=$(echo "$b" | cut -d'|' -f2 | sed 's/^ *//;s/ *$//')
        bv=$(echo "$b" | cut -d'|' -f3 | sed 's/^ *//;s/ *$//')
        ov=$(echo "$o" | cut -d'|' -f3 | sed 's/^ *//;s/ *$//')
        echo "| $name | $bv | $ov |"
    done
} | tee "$out"
rm -f "$out.berth" "$out.orbstack"
rm -rf "$WORK"

# Leave things as they usually are here: OrbStack stopped, berth running and
# owning the docker context.
stop_engine orbstack
start_engine berth
