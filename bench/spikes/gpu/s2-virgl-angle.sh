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
            (cd "$out/angle" && python3 scripts/bootstrap.py && gclient sync)
        fi
        (cd "$out/angle" &&
            gn gen out/Release --args='is_debug=false is_component_build=false angle_enable_metal=true angle_enable_vulkan=false angle_enable_gl=false angle_enable_swiftshader=false angle_build_tests=false' &&
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

    # libepoxy with EGL (Homebrew's has only CGL).
    if [ ! -f "$prefix/lib/libepoxy.dylib" ]; then
        [ -d "$out/libepoxy" ] || git clone --depth 1 --branch "$EPOXY_TAG" https://github.com/anholt/libepoxy.git "$out/libepoxy"
        PKG_CONFIG_PATH="$prefix/lib/pkgconfig" meson setup "$out/libepoxy/build" "$out/libepoxy" --prefix "$prefix" \
            -Degl=yes -Dglx=no -Dx11=false -Dtests=false --wipe
        ninja -C "$out/libepoxy/build" install
    fi

    # virglrenderer, EGL only (no GBM, no DRM on macOS).
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
RUSTFLAGS="-L $prefix/lib" DYLD_LIBRARY_PATH="$prefix/lib" \
    cargo test --release -p berth --features virgl -- --ignored --nocapture gpu::virgl::tests::probe_renderer
echo "S2 PASS: virglrenderer started on ANGLE (Metal) and offers VIRGL2"
