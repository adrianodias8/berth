//! Where Docker keeps images: `"image_store"` in ~/.berth/config.json,
//! applied when the agent starts.
//!
//! "classic" (the default) is Docker's overlay2 store, as OrbStack uses.
//! "containerd" is containerd's image store, Docker 29's own default: it
//! holds multi-platform images and build attestations, but every build
//! compresses its layers and then unpacks them again, ~1.2 s more per
//! `docker build` (bench/results/2026-10-25-showcase.md). Each store keeps
//! its own images: after a switch, the other store's images aren't listed
//! (they stay on disk, and come back when switched back).

use serde_json::{Map, Value, json};

const FEATURE: &str = "containerd-snapshotter";

/// The `image_store` setting: true for containerd's store.
pub fn containerd() -> bool {
    crate::setting("image_store").is_some_and(|v| v == "containerd")
}

/// Our key in daemon.json, always written: without it Docker 29 picks
/// containerd's store only on a new install, and otherwise keeps the store
/// it already has data in.
pub fn apply(config: &mut Map<String, Value>, containerd: bool) {
    let features = config.entry("features").or_insert_with(|| json!({}));
    if let Some(features) = features.as_object_mut() {
        features.insert(FEATURE.into(), Value::Bool(containerd));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn keeps_other_features() {
        let mut c = config(json!({"dns": ["x"], "features": {"buildkit": true}}));
        apply(&mut c, false);
        assert_eq!(Value::Object(c.clone()), json!({"dns": ["x"], "features": {"buildkit": true, FEATURE: false}}));
        let before = c.clone();
        apply(&mut c, false);
        assert_eq!(c, before, "idempotent, so dockerd isn't restarted");
        apply(&mut c, true);
        assert_eq!(Value::Object(c), json!({"dns": ["x"], "features": {"buildkit": true, FEATURE: true}}));
    }
}
