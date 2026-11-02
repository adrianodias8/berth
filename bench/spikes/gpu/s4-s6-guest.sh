#!/bin/bash
# Spikes S4 and S6, the guest's side (pci-probe.py over the VM's view of the
# device's PCI BARs). The repository is under the Mac home folder, which
# the VM sees at the same path, so the script runs from here.
#
#   bench/spikes/gpu/s4-s6-guest.sh s6   config-space write behaviour
#   bench/spikes/gpu/s4-s6-guest.sh s4   host memory mapped into the shared region
#
# S4 needs ~/.berth/gpu/spike-s4 to exist when the guest driver starts
# (create it, restart berth, then run this); README.md has the steps.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
BERTH="${BERTH:-$(cd "$here/../../.." && pwd)/target/release/berth}"
"$BERTH" shell -- sudo python3 -I "$here/pci-probe.py" "${1:-s6}"
if [ "${1:-s6}" = s4 ]; then
    grep 'gpu: S4' "$HOME/.berth/log/daemon.log" | tail -2 | sed 's/^/HOST  /'
fi
