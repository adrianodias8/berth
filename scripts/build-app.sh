#!/bin/sh
# Build Berth.app: the SwiftUI app with the signed berth CLI inside.
# Output: target/Berth.app
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
# Use Xcode's toolchain even if the Command Line Tools are selected/broken.
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

"$root/scripts/build.sh"
# SwiftPM's default build system compiles SwiftTerm's Metal shader, which
# needs the Metal Toolchain (a separate Xcode download); the native one doesn't.
build_system=""
if ! xcrun metal --version >/dev/null 2>&1; then
    echo "note: no Metal Toolchain; building with SwiftPM's native build system" >&2
    build_system="--build-system native"
fi
swift build -c release --package-path "$root/app" $build_system

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

# A virgl build (build.sh, BERTH_VIRGL) links virglrenderer and ANGLE,
# normally as @rpath/…: copy them into Frameworks, which the binary's first
# rpath (@executable_path/../Frameworks, build.rs) points at, and drop its
# rpath to the build prefix so the app never loads them from there. The
# folder is new each time (the bundle is), so a build without virgl has none.
helper="$app/Contents/Helpers/berth"
fw="$app/Contents/Frameworks"
if otool -L "$helper" | grep -q '/libvirglrenderer'; then
    mkdir -p "$fw"
    # Where @rpath/… resolves at build time: the binary's absolute rpaths.
    rpaths=$(otool -l "$helper" | awk '$1 == "path" && $2 ~ /^\// {print $2}')
    # Mach-O files whose dependencies to bundle, the binary first; then
    # every library copied. libepoxy dlopens libEGL and libGLESv2
    # (@rpath/…), and libEGL libGLESv2 from its own folder: not in
    # `otool -L`, so they're named here.
    todo="$helper"
    for leaf in libEGL.dylib libGLESv2.dylib; do
        for dir in $rpaths; do
            if [ -f "$dir/$leaf" ]; then cp -L "$dir/$leaf" "$fw/$leaf" && todo="$todo $fw/$leaf" && break; fi
        done
        [ -f "$fw/$leaf" ] || { echo "error: $leaf isn't in the rpaths of $helper ($rpaths)" >&2; exit 1; }
    done
    while [ -n "$todo" ]; do
        next=""
        for file in $todo; do
            self=$(otool -D "$file" | tail -n +2)
            for dep in $(otool -L "$file" | tail -n +2 | awk '{print $1}'); do
                [ "$dep" != "$self" ] || continue
                case "$dep" in
                    @rpath/*) leaf="${dep#@rpath/}"; src="" ;;
                    # A library meson installed under its absolute path
                    # (bench/spikes/gpu/s2-virgl-angle.sh names fixes the
                    # prefix): point the reference at the bundled copy.
                    /System/* | /usr/lib/*) continue ;;
                    /*)
                        leaf="${dep##*/}"
                        src="$dep"
                        install_name_tool -change "$dep" "@rpath/$leaf" "$file"
                        ;;
                    *) continue ;;
                esac
                [ ! -f "$fw/$leaf" ] || continue
                if [ -z "$src" ]; then
                    for dir in $rpaths; do
                        if [ -f "$dir/$leaf" ]; then src="$dir/$leaf"; break; fi
                    done
                fi
                [ -n "$src" ] || { echo "error: @rpath/$leaf (from $file) isn't in $rpaths" >&2; exit 1; }
                cp -L "$src" "$fw/$leaf"
                next="$next $fw/$leaf"
            done
        done
        todo="$next"
    done
    for lib in "$fw"/*.dylib; do
        leaf="${lib##*/}"
        install_name_tool -id "@rpath/$leaf" "$lib"
        # libepoxy's dlopen of @rpath/libEGL and virglrenderer's libepoxy
        # resolve next to themselves, whichever binary loads them.
        case "$leaf" in
            libepoxy* | libvirglrenderer*)
                if [ -z "$(otool -l "$lib" | awk '$1 == "path" && $2 == "@loader_path"')" ]; then
                    install_name_tool -add_rpath @loader_path "$lib"
                fi
                ;;
        esac
    done
    for dir in $rpaths; do
        install_name_tool -delete_rpath "$dir" "$helper"
    done
    # Ad-hoc, like the rest: no hardened runtime, so no library validation.
    for lib in "$fw"/*.dylib; do
        codesign --force --sign - --timestamp=none "$lib"
    done
    echo "bundled $(cd "$fw" && echo *.dylib)"
fi

# Inner binary first (it needs the virtualization entitlement), then the bundle.
codesign --force --sign - --timestamp=none --entitlements "$root/berth.entitlements" "$helper"
codesign --force --sign - "$app"
echo "built $app"
