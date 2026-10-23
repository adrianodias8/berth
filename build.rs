//! Embed the guest agent, which scripts/build.sh cross-compiles first.

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
}
