#!/bin/bash
# Everyday workloads on a running engine, by Docker context:
#
#   bench/workloads.sh berth        # or: orbstack
#
# Unlike compare.sh it neither starts nor stops engines, so run it on each
# engine in turn with the other idle. Prints one Markdown table row per
# workload. Internet-bound rows (download, pull) depend on the connection:
# compare runs taken back to back.
set -u
CTX=${1:?context}
WORK=$HOME/.berth-bench-workloads
rm -rf "$WORK"; mkdir -p "$WORK"
d() { docker --context "$CTX" "$@"; }
now() { python3 -c 'import time; print(time.time())'; }
since() { python3 -c "import sys; print(f'{float(sys.argv[2]) - float(sys.argv[1]):.2f}')" "$1" "$(now)"; }
row() { echo "| $CTX | $1 | $2 |"; }
calc() { python3 -c "print($1)" 2>/dev/null || echo "?"; }
# Run a script in a container with $WORK at /b; it prints milliseconds.
in_container() { local script=$1; shift; d run --rm "$@" -v "$WORK:/b" alpine:3.22 sh "/b/$script"; }

# busybox date has no %N: milliseconds since boot.
cat > "$WORK/ms.sh" <<'EOF'
ms() { awk '{ printf "%d", $1 * 1000 }' /proc/uptime; }
EOF

d pull -q alpine:3.22 >/dev/null

# Container start: mean of 10 `docker run --rm alpine true`.
t=$(now); for _ in $(seq 10); do d run --rm alpine:3.22 true; done
row "container run (mean of 10)" "$(calc "f'{$(since "$t") / 10:.2f} s'")"

# Container <-> Mac over host.docker.internal: an HTTP server on the Mac that
# streams 2 GiB for GET and discards POST bodies.
cat > "$WORK/server.py" <<'EOF'
import http.server, sys
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200); self.send_header("Content-Length", str(2 << 30)); self.end_headers()
        buf = b"\0" * (1 << 20)
        for _ in range(2048): self.wfile.write(buf)
    def do_POST(self):
        left = int(self.headers["Content-Length"])
        while left: left -= len(self.rfile.read(min(left, 1 << 20)))
        self.send_response(204); self.send_header("Content-Length", "0"); self.end_headers()
s = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
open(sys.argv[1], "w").write(str(s.server_address[1]))
s.serve_forever()
EOF
python3 "$WORK/server.py" "$WORK/port" & server=$!
for _ in $(seq 50); do [ -s "$WORK/port" ] && break; sleep 0.1; done
port=$(cat "$WORK/port")
cat > "$WORK/down.sh" <<EOF
. /b/ms.sh; t=\$(ms); wget -q -O /dev/null http://host.docker.internal:$port/; echo \$(( \$(ms) - t ))
EOF
cat > "$WORK/up.sh" <<EOF
. /b/ms.sh; head -c 1073741824 /dev/zero > /tmp/f; t=\$(ms); wget -q -O /dev/null --post-file=/tmp/f http://host.docker.internal:$port/; echo \$(( \$(ms) - t ))
EOF
ms=$(in_container down.sh); row "Mac -> container (2 GiB HTTP, host.docker.internal)" "$(calc "f'{16 / ($ms / 1000):.1f} Gbit/s'")"
ms=$(in_container up.sh); row "container -> Mac (1 GiB HTTP POST)" "$(calc "f'{8 / ($ms / 1000):.1f} Gbit/s'")"
kill $server 2>/dev/null

# Internet download through the VM's NAT.
cat > "$WORK/inet.sh" <<'EOF'
. /b/ms.sh; t=$(ms); wget -q -O /dev/null https://ash-speed.hetzner.com/100MB.bin; echo $(( $(ms) - t ))
EOF
ms=$(in_container inet.sh); row "internet download (100 MB)" "$(calc "f'{800 / ($ms / 1000):.0f} Mbit/s'")"

# DNS: 200 lookups of an external name (cached upstream: the resolver path).
cat > "$WORK/dns.sh" <<'EOF'
. /b/ms.sh; t=$(ms); i=0; while [ $i -lt 200 ]; do getent hosts example.com >/dev/null; i=$((i+1)); done; echo $(( $(ms) - t ))
EOF
ms=$(in_container dns.sh); row "DNS lookup (mean of 200, example.com)" "$(calc "f'{$ms / 200:.2f} ms'")"

# Image pull (network + unpack), removed first.
d rmi -f node:22-alpine >/dev/null 2>&1
t=$(now); d pull -q node:22-alpine >/dev/null; row "pull node:22-alpine" "$(since "$t") s"

# docker build with a 5,000-file context from the Mac.
mkdir -p "$WORK/ctx/src"
python3 -c "
for i in range(5000):
    open('$WORK/ctx/src/f%04d.txt' % i, 'w').write('x' * 2048)
"
printf 'FROM alpine:3.22\nRUN apk add --no-cache python3 >/dev/null\nCOPY src /src\nRUN find /src -type f | wc -l\n' > "$WORK/ctx/Dockerfile"
t=$(now); d build -q --no-cache -t berth-bench-build "$WORK/ctx" >/dev/null; row "docker build (5k-file context, no cache)" "$(since "$t") s"
d rmi -f berth-bench-build >/dev/null 2>&1

# Docker volume I/O inside the VM: 1 GiB write+fsync, cold read, 20k files.
cat > "$WORK/vol.sh" <<'EOF'
. /b/ms.sh
t=$(ms); dd if=/dev/zero of=/v/big bs=1M count=1024 conv=fsync 2>/dev/null; w=$(( $(ms) - t ))
sync; echo 3 > /proc/sys/vm/drop_caches
t=$(ms); dd if=/v/big of=/dev/null bs=1M 2>/dev/null; r=$(( $(ms) - t ))
t=$(ms); mkdir /v/s; i=0; while [ $i -lt 20000 ]; do echo x > /v/s/f$i; i=$((i+1)); done; s=$(( $(ms) - t ))
echo $w $r $s
EOF
d volume rm -f berth-bench-vol >/dev/null 2>&1
d volume create berth-bench-vol >/dev/null
set -- $(in_container vol.sh --privileged -v berth-bench-vol:/v)
row "volume write 1 GiB (fsync)" "$(calc "f'{1024 / ($1 / 1000):.0f} MB/s'")"
row "volume read 1 GiB (cold)" "$(calc "f'{1024 / ($2 / 1000):.0f} MB/s'")"
row "volume create 20k small files (shell loop)" "$(calc "f'{$3 / 1000:.2f} s'")"
d volume rm -f berth-bench-vol >/dev/null 2>&1
rm -rf "$WORK"
