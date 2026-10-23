# Runs in the guest: python3 latency.py <file>. Per-request round trip:
# stat() of a file and lookups of missing names, 20,000 each. Start berth
# with BERTH_FS_ENTRY_TIMEOUT=0 BERTH_FS_ATTR_TIMEOUT=0
# BERTH_FS_NEGATIVE_TIMEOUT=0 so every call reaches the server.
import os, sys, time
p, n = sys.argv[1], 20000
t = time.perf_counter()
for _ in range(n):
    os.stat(p)
s = time.perf_counter() - t
t = time.perf_counter()
for i in range(n):
    try:
        os.stat(f"{p}.missing{i}")
    except FileNotFoundError:
        pass
m = time.perf_counter() - t
print(f"stat {s / n * 1e6:.1f} µs   missing-name lookup {m / n * 1e6:.1f} µs")
