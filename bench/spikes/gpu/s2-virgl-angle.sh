#!/bin/bash
# Spike S2: virglrenderer runs on the Mac through ANGLE's Metal backend.
# Builds ANGLE (libEGL, libGLESv2), libepoxy against ANGLE's EGL and
# virglrenderer on top, into target/spikes/gpu, then runs berth's probe
# test (src/gpu/virgl.rs, probe_renderer): the renderer must start on
# ANGLE and offer the VIRGL2 capset.
#
#   bench/spikes/gpu/s2-virgl-angle.sh          build what's missing, then probe
#   bench/spikes/gpu/s2-virgl-angle.sh probe    only the probe
#
# Needs Xcode, ~10 GB of disk and an hour for ANGLE's first build. Versions
# are pinned; record what had to change in docs/LEARNINGS.md.
set -euo pipefail
root="$(cd "$(dirname "$0")/../../.." && pwd)"
out="$root/target/spikes/gpu"
prefix="$out/prefix"
ANGLE_REV="${ANGLE_REV:-chromium/7258}"
EPOXY_TAG="${EPOXY_TAG:-1.5.10}"
VIRGL_TAG="${VIRGL_TAG:-1.1.1}"
mkdir -p "$out" "$prefix/lib" "$prefix/include"
export PATH=~/.rustup/toolchains/stable-aarch64-apple-darwin/bin:~/.cargo/bin:$PATH

build() {
    command -v meson >/dev/null && command -v ninja >/dev/null || {
        echo "needs meson and ninja (brew install meson ninja pkg-config)" >&2; exit 1; }

    # ANGLE, Metal only.
    if [ ! -f "$prefix/lib/libEGL.dylib" ]; then
        [ -d "$out/depot_tools" ] || git clone --depth 1 https://chromium.googlesource.com/chromium/tools/depot_tools.git "$out/depot_tools"
        export PATH="$out/depot_tools:$PATH"
        if [ ! -d "$out/angle" ]; then
            git clone https://chromium.googlesource.com/angle/angle "$out/angle"
            git -C "$out/angle" checkout "$ANGLE_REV"
            (cd "$out/angle" && python3 scripts/bootstrap.py)
        fi
        # Until it's built, sync again: googlesource answers a burst of
        # parallel clones with HTTP 429, so few jobs, no history, retries.
        for try in 1 2 3 4; do
            (cd "$out/angle" && gclient sync -j 2 --no-history --shallow) && break
            [ "$try" = 4 ] && exit 1
            echo "gclient sync failed (try $try); waiting before the next" >&2
            sleep $((try * 120))
        done
        # Xcode's clang and the SDK's libc++: ANGLE's own clang (LLVM 21 pre-release
        # at chromium/7258) can't compile the macOS 27 SDK's Metal headers.
        # ANGLE calls Xcode's metal and metallib by path, a stub that can't
        # see the Metal Toolchain (a separate download since Xcode 26:
        # xcodebuild -downloadComponent MetalToolchain); xcrun finds it.
        xcrun -f metal >/dev/null 2>&1 || { echo "needs the Metal Toolchain: xcodebuild -downloadComponent MetalToolchain" >&2; exit 1; }
        sed -i '' 's|return subprocess.run(args,|return subprocess.run(["xcrun", os.path.basename(args[0])] + args[1:],|' \
            "$out/angle/src/libANGLE/renderer/metal/shaders/metal_wrapper.py"
        # Its other tools (llvm-ar) aren't in Xcode: a toolchain folder of
        # Xcode's clang and ANGLE's bundled LLVM tools. Linking is Xcode's
        # ld (use_lld=false): the bundled lld can't read the macOS 27 SDK's
        # .tbd stubs ("unknown architecture").
        tc="$out/toolchain/bin"
        mkdir -p "$tc"
        for t in "$out/angle/third_party/llvm-build/Release+Asserts/bin/"*; do ln -sf "$t" "$tc/"; done
        # lib/: the bundled clang's runtime (lib/clang), Xcode's libLTO for its ld.
        [ -L "$out/toolchain/lib" ] && rm "$out/toolchain/lib"
        mkdir -p "$out/toolchain/lib"
        ln -sfn "$out/angle/third_party/llvm-build/Release+Asserts/lib/clang" "$out/toolchain/lib/clang"
        ln -sf "$DEVELOPER_DIR/Toolchains/XcodeDefault.xctoolchain/usr/lib/libLTO.dylib" "$out/toolchain/lib/libLTO.dylib"
        for t in clang clang++; do ln -sf "$DEVELOPER_DIR/Toolchains/XcodeDefault.xctoolchain/usr/bin/$t" "$tc/$t"; done
        (cd "$out/angle" &&
            gn gen out/Release --args='is_debug=false is_component_build=false angle_enable_metal=true angle_enable_vulkan=false angle_enable_gl=false angle_enable_swiftshader=false angle_build_tests=false use_custom_libcxx=false treat_warnings_as_errors=false clang_base_path="'"$out/toolchain"'" clang_use_chrome_plugins=false use_lld=false' &&
            ninja -C out/Release libEGL libGLESv2)
        cp "$out/angle/out/Release/libEGL.dylib" "$out/angle/out/Release/libGLESv2.dylib" "$prefix/lib/"
        cp -R "$out/angle/include/EGL" "$out/angle/include/GLES2" "$out/angle/include/GLES3" "$out/angle/include/KHR" "$prefix/include/"
        mkdir -p "$prefix/lib/pkgconfig"
        for pc in egl:EGL glesv2:GLESv2; do
            cat > "$prefix/lib/pkgconfig/${pc%%:*}.pc" <<PC
prefix=$prefix
Name: ${pc%%:*}
Description: ANGLE ${pc##*:}
Version: 1.5
Libs: -L\${prefix}/lib -l${pc##*:}
Cflags: -I\${prefix}/include
PC
        done
    fi

    # ANGLE names its dylibs ./libEGL.dylib: @rpath instead, so whatever
    # links or dlopens them finds them through its own rpath (DYLD_* is
    # dropped through scripts/sign.sh, a shell script). Re-sign after.
    for lib in libEGL libGLESv2; do
        if [ "$(otool -D "$prefix/lib/$lib.dylib" | tail -1)" != "@rpath/$lib.dylib" ]; then
            install_name_tool -id "@rpath/$lib.dylib" "$prefix/lib/$lib.dylib"
            codesign -f -s - "$prefix/lib/$lib.dylib"
        fi
    done

    # libepoxy with EGL (Homebrew's has only CGL).
    if [ ! -f "$prefix/lib/libepoxy.dylib" ]; then
        [ -d "$out/libepoxy" ] || git clone --depth 1 --branch "$EPOXY_TAG" https://github.com/anholt/libepoxy.git "$out/libepoxy"
        # libepoxy has no EGL on Apple, and looks for .so names there: turn
        # EGL on and load ANGLE's dylibs through the loading binary's rpath.
        sed -i '' '/defined(__APPLE__)/,/PLATFORM_HAS_WGL/s/#define PLATFORM_HAS_EGL 0/#define PLATFORM_HAS_EGL ENABLE_EGL/' "$out/libepoxy/src/dispatch_common.h"
        if ! grep -q '"@rpath/libEGL.dylib"' "$out/libepoxy/src/dispatch_common.c"; then
            sed -i '' '/^#if defined(__APPLE__)$/,/^#elif/{
s/#define GLES2_LIB "libGLESv2.so"/#define GLES2_LIB "@rpath\/libGLESv2.dylib"\
#define EGL_LIB "@rpath\/libEGL.dylib"/
}' "$out/libepoxy/src/dispatch_common.c"
        fi
        PKG_CONFIG_PATH="$prefix/lib/pkgconfig" meson setup "$out/libepoxy/build" "$out/libepoxy" --prefix "$prefix" \
            -Degl=yes -Dglx=no -Dx11=false -Dtests=false --wipe
        ninja -C "$out/libepoxy/build" install
    fi

    # virglrenderer, EGL only (no GBM, no DRM on macOS). Its generators
    # need PyYAML, which Homebrew's Python doesn't have: a venv of our own.
    [ -x "$out/venv/bin/python3" ] || { /opt/homebrew/bin/python3 -m venv "$out/venv" && "$out/venv/bin/pip" install -q pyyaml; }
    export PATH="$out/venv/bin:$PATH"
    if [ ! -f "$prefix/lib/libvirglrenderer.dylib" ]; then
        [ -d "$out/virglrenderer" ] || git clone --depth 1 --branch "$VIRGL_TAG" https://gitlab.freedesktop.org/virgl/virglrenderer.git "$out/virglrenderer"
        PKG_CONFIG_PATH="$prefix/lib/pkgconfig" meson setup "$out/virglrenderer/build" "$out/virglrenderer" --prefix "$prefix" \
            -Dplatforms=egl -Dminigbm_allocation=false -Dvenus=false -Dtests=false --wipe
        ninja -C "$out/virglrenderer/build" install
    fi
}

[ "${1:-}" = probe ] || build
# The probe: berth's own FFI, linked against what was just built.
cd "$root"
RUSTFLAGS="-L $prefix/lib -C link-arg=-Wl,-rpath,$prefix/lib" \
    cargo test --release -p berth --features virgl -- --ignored --nocapture gpu::virgl::tests::probe_renderer
echo "S2 PASS: virglrenderer started on ANGLE (Metal) and offers VIRGL2"
