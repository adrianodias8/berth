#!/bin/sh
# Build Berth.app: the SwiftUI app with the signed berth CLI inside.
# Output: target/Berth.app
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
# Use Xcode's toolchain even if the Command Line Tools are selected/broken.
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

"$root/scripts/build.sh"
swift build -c release --package-path "$root/app"

app="$root/target/Berth.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Helpers" "$app/Contents/Resources"
cp "$root/app/Resources/Info.plist" "$app/Contents/Info.plist"
cp "$root/app/.build/release/Berth" "$app/Contents/MacOS/Berth"
# Helpers/, not MacOS/: "berth" and "Berth" collide on a case-insensitive disk.
cp "$root/target/release/berth" "$app/Contents/Helpers/berth"

iconset="$root/target/AppIcon.iconset"
if [ ! -f "$root/target/AppIcon.icns" ] || [ "$root/app/Resources/make-icon.swift" -nt "$root/target/AppIcon.icns" ]; then
    rm -rf "$iconset"
    swift "$root/app/Resources/make-icon.swift" "$iconset"
    iconutil -c icns "$iconset" -o "$root/target/AppIcon.icns"
    rm -rf "$iconset"
fi
cp "$root/target/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"

# berth's guest kernel, when built (guest/kernel/build.sh; it takes a while,
# so it isn't part of this script). Without it the VM boots its Debian kernel.
rm -rf "$app/Contents/Resources/kernel"
if [ -f "$root/target/kernel/Image" ]; then
    mkdir -p "$app/Contents/Resources/kernel"
    cp "$root/target/kernel/Image" "$root/target/kernel/release" "$root/target/kernel/modules.tar" "$app/Contents/Resources/kernel/"
fi

# Inner binary first (it needs the virtualization entitlement), then the bundle.
codesign --force --sign - --entitlements "$root/berth.entitlements" "$app/Contents/Helpers/berth"
codesign --force --sign - "$app"
echo "built $app"
