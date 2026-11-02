//! Embed the guest agents, which scripts/build.sh builds first, and with the
//! `virgl` feature point the linker at virglrenderer and ANGLE.

use std::path::{Path, PathBuf};

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let agent = Path::new(&manifest).join("target/aarch64-unknown-linux-musl/release/berth-agent");
    println!("cargo:rerun-if-changed={}", agent.display());
    if !agent.exists() {
        panic!(
            "guest agent not built; run scripts/build.sh, or:\n  \
             cargo build -p berth-agent --release --target aarch64-unknown-linux-musl"
        );
    }
    println!("cargo:rustc-env=BERTH_AGENT={}", agent.display());

    // berth-gui-mac, for macOS VMs (src/macvm/guest.rs). Optional: without
    // Swift, berth embeds an empty file and says so when it would install it.
    let gui_mac = Path::new(&manifest).join("guest/mac-agent/.build/release/berth-gui-mac");
    println!("cargo:rerun-if-changed={}", gui_mac.display());
    let embedded = if gui_mac.exists() {
        gui_mac
    } else {
        let empty = Path::new(&std::env::var("OUT_DIR").unwrap()).join("berth-gui-mac-missing");
        std::fs::write(&empty, b"").unwrap();
        empty
    };
    println!("cargo:rustc-env=BERTH_GUI_MAC={}", embedded.display());

    if std::env::var_os("CARGO_FEATURE_VIRGL").is_some() {
        virgl(Path::new(&manifest));
    }
}

/// Link against the prefix bench/spikes/gpu/s2-virgl-angle.sh builds. The
/// libraries are named `@rpath/…`, so the binary carries two rpaths: the
/// app bundle's Frameworks first (scripts/build-app.sh copies them there and
/// drops the second), then the prefix, so a development build and
/// `cargo test` find them with no DYLD_* or RUSTFLAGS.
fn virgl(manifest: &Path) {
    println!("cargo:rerun-if-env-changed=BERTH_VIRGL_PREFIX");
    let prefix = std::env::var_os("BERTH_VIRGL_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("target/spikes/gpu/prefix"));
    let lib = prefix.join("lib");
    println!("cargo:rerun-if-changed={}", lib.display());

    let names: Vec<String> = std::fs::read_dir(&lib)
        .map(|dir| dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let has = |stem: &str| names.iter().any(|n| n.starts_with(stem) && n.ends_with(".dylib"));
    let missing: Vec<&str> =
        ["libvirglrenderer", "libepoxy", "libEGL.dylib", "libGLESv2.dylib"].into_iter().filter(|s| !has(s)).collect();
    if !missing.is_empty() {
        panic!(
            "the virgl feature needs virglrenderer, libepoxy and ANGLE in {} (missing {}); \
             run bench/spikes/gpu/s2-virgl-angle.sh or set BERTH_VIRGL_PREFIX",
            lib.display(),
            missing.join(", ")
        );
    }

    // meson installs virglrenderer and libepoxy under their absolute paths;
    // linked like that, the binary only runs next to this prefix.
    let id = std::process::Command::new("otool").arg("-D").arg(lib.join("libvirglrenderer.dylib")).output();
    if let Ok(out) = id
        && !String::from_utf8_lossy(&out.stdout).contains("@rpath/")
    {
        println!(
            "cargo:warning=libvirglrenderer in {} isn't named @rpath/...; \
             run bench/spikes/gpu/s2-virgl-angle.sh names",
            lib.display()
        );
    }

    println!("cargo:rustc-link-search=native={}", lib.display());
    // Weakly: a copy of the binary without them next to it still starts
    // (`berth helper install` copies it to /Library/PrivilegedHelperTools),
    // and Renderer::get finds them missing instead of dyld aborting.
    println!("cargo:rustc-link-arg=-Wl,-weak-lvirglrenderer");
    println!("cargo:rustc-link-arg=-Wl,-weak-lEGL");
    println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
}
