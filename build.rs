//! Embed the guest agents, which scripts/build.sh builds first.

use std::path::Path;

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
}
