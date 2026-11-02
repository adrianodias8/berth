#!/usr/bin/env python3
# Runs in the VM, as root, for spikes S4 and S6: finds berth's virtio-gpu on
# PCI (vendor 0x1af4, device 0x1050 = 0x1040 + virtio ID 16), walks its
# virtio PCI capabilities, and pokes the regions they point to through the
# BAR's sysfs resource file.
#
#   pci-probe.py s6   config space: read it, write events_clear, read it back
#   pci-probe.py s4   shared memory region: read what the host mapped there
#
# Only reads and the one events_clear write the guest driver itself makes.
import mmap
import os
import struct
import sys

CAP_VENDOR = 0x09
CFG_TYPES = {1: "common", 2: "notify", 3: "isr", 4: "device", 5: "pci", 8: "shared-memory"}


def find_device():
    for dev in sorted(os.listdir("/sys/bus/pci/devices")):
        path = f"/sys/bus/pci/devices/{dev}"
        ids = [open(f"{path}/{f}").read().strip() for f in ("vendor", "device")]
        if ids == ["0x1af4", "0x1050"]:
            return path
    sys.exit("no virtio-gpu PCI device (is \"gui\": true set and berth restarted?)")


def capabilities(path):
    config = open(f"{path}/config", "rb").read()
    if len(config) < 256:
        sys.exit("can't read the full PCI config space: run as root")
    caps, at = [], config[0x34]
    while at:
        cap_id, nxt, length = config[at], config[at + 1], config[at + 2]
        if cap_id == CAP_VENDOR:
            cfg_type, bar, shm_id = config[at + 3], config[at + 4], config[at + 5]
            offset, size = struct.unpack_from("<II", config, at + 8)
            if cfg_type == 8 and length >= 24:
                hi_off, hi_len = struct.unpack_from("<II", config, at + 16)
                offset |= hi_off << 32
                size |= hi_len << 32
            caps.append({"type": CFG_TYPES.get(cfg_type, cfg_type), "bar": bar, "id": shm_id, "offset": offset, "size": size})
        at = nxt
    return caps


def window(path, cap, length):
    """An mmap of `length` bytes at the capability's offset in its BAR."""
    page = mmap.PAGESIZE
    base = cap["offset"] - cap["offset"] % page
    fd = os.open(f"{path}/resource{cap['bar']}", os.O_RDWR | os.O_SYNC)
    m = mmap.mmap(fd, length + cap["offset"] - base, mmap.MAP_SHARED, offset=base)
    return m, cap["offset"] - base


def s6(path, caps):
    dev = next((c for c in caps if c["type"] == "device"), None)
    if not dev:
        sys.exit("FAIL  no device-config capability")
    m, at = window(path, dev, 16)
    view = memoryview(m)[at:at + 16].cast("I")
    before = list(view)
    print(f"INFO  config space: events_read={before[0]} events_clear={before[1]} num_scanouts={before[2]} num_capsets={before[3]}")
    print("PASS  num_scanouts is 1" if before[2] == 1 else f"FAIL  num_scanouts {before[2]}")
    print("PASS  events_read is 0 (berth never raises it)" if before[0] == 0 else f"INFO  events_read {before[0]}")
    view[1] = 1  # what virtio_gpu writes to acknowledge a display event
    after = list(view)
    print(f"INFO  after writing events_clear=1: events_read={after[0]} events_clear={after[1]}")
    print("RESULT the write " + ("is stored and reads back" if after[1] == 1 else "is dropped (reads back 0)"))
    view[1] = 0
    view.release()
    m.close()


def s4(path, caps):
    shm = next((c for c in caps if c["type"] == "shared-memory" and c["id"] == 1), None)
    if not shm:
        sys.exit("FAIL  no host-visible shared memory capability (id 1)")
    print(f"PASS  host-visible region: BAR {shm['bar']}, offset {shm['offset']:#x}, {shm['size'] >> 20} MiB")
    m, at = window(path, shm, 4096)
    head = bytes(m[at:at + 16])
    m.close()
    print(f"INFO  first bytes: {head!r}")
    print("PASS  the host's pages are visible (berth-s4 pattern)" if head.startswith(b"berth-s4") else
          "FAIL  pattern not there (was ~/.berth/gpu/spike-s4 present when the guest driver started?)")


def main():
    what = sys.argv[1] if len(sys.argv) > 1 else "s6"
    path = find_device()
    caps = capabilities(path)
    print(f"INFO  {path}")
    for c in caps:
        print(f"INFO  cap {c['type']}: bar {c['bar']} id {c['id']} offset {c['offset']:#x} size {c['size']:#x}")
    {"s6": s6, "s4": s4}[what](path, caps)


main()
