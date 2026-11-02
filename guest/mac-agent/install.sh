#!/bin/sh
# Installs berth-gui-mac into the current user's GUI session in a macOS guest,
# as the LaunchAgent dev.berth.gui-mac. Run it inside the VM, as the user who
# logs in automatically (not root), once in the base image: clones inherit the
# binary, the LaunchAgent and the privacy grants.
#
#   ./install.sh [path/to/berth-gui-mac]   install or update, then (re)start it
#   ./install.sh --status                  is it running, and does it have its grants
#   ./install.sh --uninstall               stop it and remove it
#
# The binary is built on the Mac (swift build -c release in guest/mac-agent)
# and copied into the guest; the guest needs no Xcode.
#
# One-time grants, in the base image, through the VM's display window:
#   1. After the first start, macOS asks for Accessibility and for Screen
#      Recording on behalf of berth-gui-mac. Open System Settings >
#      Privacy & Security and switch berth-gui-mac on under Accessibility and
#      under Screen & System Audio Recording (this script opens both panes).
#   2. Restart it so Screen Recording takes effect: ./install.sh again, or
#      launchctl kickstart -k gui/$(id -u)/dev.berth.gui-mac
#   3. ./install.sh --status should then say both are granted.
# The grants can't be scripted: TCC only takes them from a person clicking in
# System Settings (or from an MDM profile). They're tied to the binary's code
# signature; this ad-hoc signed build changes with every rebuild, so after
# installing a new build, grant again (toggle off and on) in the base image.
set -eu

LABEL=dev.berth.gui-mac
DIR="$HOME/Library/Application Support/Berth"
BIN="$DIR/berth-gui-mac"
LOG="$HOME/Library/Logs/berth-gui-mac.log"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
HERE=$(cd "$(dirname "$0")" && pwd)
DOMAIN="gui/$(id -u)"

if [ "$(id -u)" = 0 ]; then
    echo "install.sh: run this as the guest's logged-in user, not root" >&2
    exit 1
fi

status() {
    if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
        echo "$LABEL: loaded"
        launchctl print "$DOMAIN/$LABEL" | grep -E '^[[:space:]]*(state|pid) =' || true
    else
        echo "$LABEL: not loaded"
    fi
    # The agent logs its grants when it starts. Asking from this shell would
    # report the terminal's grants, not the agent's.
    if [ -f "$LOG" ]; then
        grep 'accessibility:' "$LOG" | tail -n 1 || true
    fi
}

case "${1:-}" in
--status)
    status
    exit 0
    ;;
--uninstall)
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    rm -f "$PLIST" "$BIN"
    echo "removed $LABEL"
    exit 0
    ;;
-h|--help)
    sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
esac

SRC=${1:-}
if [ -z "$SRC" ]; then
    for c in "$HERE/berth-gui-mac" "$HERE/.build/release/berth-gui-mac"; do
        if [ -x "$c" ]; then SRC=$c; break; fi
    done
fi
if [ -z "$SRC" ] || [ ! -x "$SRC" ]; then
    echo "install.sh: no berth-gui-mac binary; pass its path (build it with: swift build -c release)" >&2
    exit 1
fi

mkdir -p "$DIR" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs" "$HOME/Pushed"
# Replace, don't overwrite in place: the running copy keeps its old inode.
cp "$SRC" "$BIN.new"
chmod 755 "$BIN.new"
# A stable signing identifier; TCC lists the binary under this name.
codesign --force --sign - --identifier "$LABEL" "$BIN.new" >/dev/null 2>&1 || true
mv -f "$BIN.new" "$BIN"

# The plist holds absolute paths; escape what sed would treat specially.
esc() { printf '%s' "$1" | sed 's/[&|\\]/\\&/g'; }
sed -e "s|__BIN__|$(esc "$BIN")|" -e "s|__LOG__|$(esc "$LOG")|g" "$HERE/$LABEL.plist" > "$PLIST"
plutil -lint "$PLIST" >/dev/null

launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
if ! launchctl bootstrap "$DOMAIN" "$PLIST"; then
    echo "install.sh: launchctl bootstrap $DOMAIN failed; is this user logged in to the desktop?" >&2
    exit 1
fi
launchctl kickstart -k "$DOMAIN/$LABEL" >/dev/null 2>&1 || true
sleep 1
status

if ! grep 'accessibility:' "$LOG" 2>/dev/null | tail -n 1 | grep -q 'accessibility: true, screen recording: true'; then
    echo
    echo "Grant berth-gui-mac Accessibility and Screen Recording in System Settings,"
    echo "then run ./install.sh --status (restart it after granting Screen Recording)."
    open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility" 2>/dev/null || true
    open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture" 2>/dev/null || true
fi
