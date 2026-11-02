#!/bin/sh
# Check that Berth.app stands on its own: every library it links is inside
# it, nothing points back at the build machine, the signatures hold, and
# a virgl build loads virglrenderer and ANGLE from Contents/Frameworks.
#
#   scripts/check-bundle.sh [path/to/Berth.app]    default target/Berth.app
#
# Lists every problem it finds, and exits non-zero if there was one.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
app="${1:-$root/target/Berth.app}"
helper="$app/Contents/Helpers/berth"
fw="$app/Contents/Frameworks"
[ -x "$helper" ] || { echo "error: no $helper" >&2; exit 1; }
failed=0
fail() {
    echo "FAIL: $*" >&2
    failed=1
}

# Load commands: no rpath into a build tree, no library outside the
# system's by an absolute path, and every @rpath/… one in Frameworks.
# SwiftPM gives the app rpaths to Xcode's Swift runtimes (back-deployment
# only, unused on macOS 14+): listed, not failed.
for file in $(find "$app/Contents" -type f); do
    file -b "$file" | grep -q Mach-O || continue
    name="${file#"$app"/}"
    for path in $(otool -l "$file" | awk '$1 == "path" {print $2}'); do
        case "$path" in
            "$root"/* | */target/*) fail "$name has the rpath $path, into the build" ;;
            /usr/lib/* | /System/*) ;;
            /*) echo "note: $name has the rpath $path" ;;
        esac
    done
    self=$(otool -D "$file" | tail -n +2)
    for dep in $(otool -L "$file" | tail -n +2 | awk '{print $1}'); do
        [ "$dep" != "$self" ] || continue
        case "$dep" in
            /System/* | /usr/lib/*) ;;
            /*) fail "$name links $dep, outside the bundle" ;;
            @rpath/*) [ -f "$fw/${dep#@rpath/}" ] || fail "$name links $dep, which isn't in Contents/Frameworks" ;;
            @executable_path/* | @loader_path/*) ;;
            *) fail "$name links $dep, a relative path" ;;
        esac
    done
done

if ! codesign --verify --deep --strict "$app" 2>&1; then
    fail "codesign --verify --deep --strict $app"
fi

if ! otool -L "$helper" | grep -q '/libvirglrenderer'; then
    [ ! -d "$fw" ] || fail "no virgl, but there's a Contents/Frameworks"
    [ "$failed" = 0 ] || exit 1
    echo "ok: $app (no virgl)"
    exit 0
fi

# The renderer starts from the bundle, and dyld takes all four libraries
# from Contents/Frameworks (DYLD_PRINT_LIBRARIES works: no hardened runtime).
log=$(mktemp)
trap 'rm -f "$log" "$log.out"' EXIT
if ! DYLD_PRINT_LIBRARIES=1 "$helper" gpu-probe >"$log.out" 2>"$log"; then
    fail "berth gpu-probe failed: $(cat "$log.out")"
fi
grep -q '^capset 2 ' "$log.out" || fail "berth gpu-probe offers no capset 2 (VIRGL2): $(cat "$log.out")"
for lib in libvirglrenderer libepoxy libEGL libGLESv2; do
    # Lines are `dyld[<pid>]: <<uuid>> <path>`.
    loaded=$(grep "/$lib[.]" "$log" | awk '{print $NF}' || true)
    if [ -z "$loaded" ]; then
        fail "gpu-probe didn't load $lib"
    fi
    # dyld prints the path its rpath made (…/Helpers/../Frameworks/…).
    for path in $loaded; do
        [ "$(cd "${path%/*}" && pwd -P)" = "$(cd "$fw" && pwd -P)" ] ||
            fail "gpu-probe loaded $path, outside Contents/Frameworks"
    done
done
sed -n 's/^gl //p' "$log.out"
[ "$failed" = 0 ] || exit 1
echo "ok: $app (virgl: $(cd "$fw" && echo *.dylib))"
