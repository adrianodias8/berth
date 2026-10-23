//! The `berth` docker context, made current on start like OrbStack does with
//! its own. Written straight into the docker CLI's config directory rather
//! than via `docker context` so it works without the CLI on PATH (e.g. when
//! Berth.app starts us).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::paths::Paths;

pub const NAME: &str = "berth";

fn config_dir(paths: &Paths) -> PathBuf {
    std::env::var_os("DOCKER_CONFIG").map_or_else(|| paths.home.join(".docker"), PathBuf::from)
}

/// Create or update the context and make it the CLI's current one.
pub fn use_berth(paths: &Paths) -> Result<()> {
    let dir = config_dir(paths);

    // Contexts live in contexts/meta/<sha256 of the name>/meta.json.
    let id: String = Sha256::digest(NAME.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let meta_dir = dir.join("contexts/meta").join(id);
    fs::create_dir_all(&meta_dir)?;
    let meta = json!({
        "Name": NAME,
        "Metadata": { "Description": "berth" },
        "Endpoints": {
            "docker": {
                "Host": format!("unix://{}", paths.docker_socket().display()),
                "SkipTLSVerify": false,
            }
        }
    });
    write_atomic(&meta_dir.join("meta.json"), &serde_json::to_vec(&meta)?)?;

    // Set currentContext, keeping every other key (auths, credsStore, ...).
    let config_path = dir.join("config.json");
    let mut config: Map<String, Value> = match fs::read(&config_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("parsing ~/.docker/config.json")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e.into()),
    };
    if config.get("currentContext").and_then(Value::as_str) != Some(NAME) {
        config.insert("currentContext".into(), Value::String(NAME.into()));
        write_atomic(&config_path, &serde_json::to_vec_pretty(&config)?)?;
    }
    Ok(())
}

/// Replace via rename so a crash never leaves a half-written file. Keeps the
/// existing file's permissions (config.json can hold credentials).
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o777);
    let tmp = path.with_extension("berth-tmp");
    fs::write(&tmp, contents)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)?;
    Ok(())
}
