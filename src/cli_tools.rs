//! The Docker CLIs, so berth doesn't depend on another app's copies.
//!
//! Official builds are downloaded once into ~/.berth/bin (pinned to versions
//! matching the guest engine) and linked where tools look for them, but only
//! where nothing working is there already: a CLI from OrbStack or Docker
//! Desktop keeps working until it goes away, and then ours takes over.
//! /usr/local/bin needs root, so the route helper keeps those links healthy.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

const DOCKER: &str = "29.9.2";
const COMPOSE: &str = "v5.6.1";
const BUILDX: &str = "v0.38.1";

/// Names linked into /usr/local/bin by the helper (see helper.rs).
pub const SYSTEM_LINKS: [&str; 2] = ["docker", "docker-compose"];

pub fn bin_dir(paths: &Paths) -> PathBuf {
    paths.root.join("bin")
}

/// Download anything missing or outdated, then link the CLI plugins.
pub fn ensure(paths: &Paths) -> Result<()> {
    let bin = bin_dir(paths);
    fs::create_dir_all(&bin)?;
    let stamp = bin.join(".versions");
    let want = format!("{DOCKER} {COMPOSE} {BUILDX}\n");
    if fs::read_to_string(&stamp).ok().as_deref() != Some(want.as_str()) {
        eprintln!("Downloading Docker CLI {DOCKER}, Compose {COMPOSE} and Buildx {BUILDX}...");
        fetch_docker(&bin)?;
        fetch_verified(
            &bin.join("docker-compose"),
            &format!("https://github.com/docker/compose/releases/download/{COMPOSE}/docker-compose-darwin-aarch64"),
            Checksum::Sha256File(format!(
                "https://github.com/docker/compose/releases/download/{COMPOSE}/docker-compose-darwin-aarch64.sha256"
            )),
        )?;
        let asset = format!("buildx-{BUILDX}.darwin-arm64");
        fetch_verified(
            &bin.join("docker-buildx"),
            &format!("https://github.com/docker/buildx/releases/download/{BUILDX}/{asset}"),
            Checksum::GitHubAsset { repo: "docker/buildx", tag: BUILDX, name: asset.clone() },
        )?;
        fs::write(&stamp, want)?;
    }
    link_plugins(paths, &bin)
}

/// The CLI ships as a tarball with no published checksum; HTTPS only.
fn fetch_docker(bin: &Path) -> Result<()> {
    let tmp = bin.join("docker.tgz");
    curl(&format!("https://download.docker.com/mac/static/stable/aarch64/docker-{DOCKER}.tgz"), &tmp)?;
    let status = Command::new("tar").args(["-xzf"]).arg(&tmp).args(["-C"]).arg(bin).args(["--strip-components", "1", "docker/docker"]).status()?;
    fs::remove_file(&tmp).ok();
    if !status.success() {
        bail!("extracting the docker CLI failed");
    }
    Ok(())
}

enum Checksum {
    /// A `<hash> *<name>` file next to the binary.
    Sha256File(String),
    /// The digest GitHub records for a release asset. (Buildx's provenance
    /// digests predate Docker's macOS code signing, so they don't match.)
    GitHubAsset { repo: &'static str, tag: &'static str, name: String },
}

fn fetch_verified(dest: &Path, url: &str, checksum: Checksum) -> Result<()> {
    let tmp = dest.with_extension("download");
    curl(url, &tmp)?;
    let expected = match checksum {
        Checksum::Sha256File(sum_url) => {
            let text = curl_text(&sum_url)?;
            text.split_whitespace().next().context("empty checksum file")?.to_string()
        }
        Checksum::GitHubAsset { repo, tag, name } => {
            let release: serde_json::Value = serde_json::from_str(&curl_text(&format!(
                "https://api.github.com/repos/{repo}/releases/tags/{tag}"
            ))?)?;
            asset_digest(&release, &name).context("release asset has no digest")?
        }
    };
    let actual = sha256_file(&tmp)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        fs::remove_file(&tmp).ok();
        bail!("checksum mismatch for {url}");
    }
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

/// `assets[name == name].digest`, without its "sha256:" prefix.
fn asset_digest(release: &serde_json::Value, name: &str) -> Option<String> {
    release["assets"]
        .as_array()?
        .iter()
        .find(|a| a["name"] == name)?["digest"]
        .as_str()?
        .strip_prefix("sha256:")
        .map(str::to_string)
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn curl(url: &str, dest: &Path) -> Result<()> {
    let status = Command::new("curl").args(["--fail", "--silent", "--show-error", "--location", "-o"]).arg(dest).arg(url).status()?;
    if !status.success() {
        bail!("downloading {url} failed");
    }
    Ok(())
}

fn curl_text(url: &str) -> Result<String> {
    let out = Command::new("curl").args(["--fail", "--silent", "--location", url]).output()?;
    if !out.status.success() {
        bail!("downloading {url} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A symlink whose target no longer exists (e.g. into an uninstalled app).
pub fn is_dangling(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) && !path.exists()
}

/// docker finds compose and buildx in ~/.docker/cli-plugins.
fn link_plugins(paths: &Paths, bin: &Path) -> Result<()> {
    let plugins = std::env::var_os("DOCKER_CONFIG")
        .map_or_else(|| paths.home.join(".docker"), PathBuf::from)
        .join("cli-plugins");
    fs::create_dir_all(&plugins)?;
    for name in ["docker-compose", "docker-buildx"] {
        let link = plugins.join(name);
        if fs::symlink_metadata(&link).is_err() || is_dangling(&link) {
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(bin.join(name), &link)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_release_asset_digest() {
        let release = serde_json::json!({"assets": [
            {"name": "other", "digest": "sha256:000"},
            {"name": "buildx-v1.darwin-arm64", "digest": "sha256:6f20ba"}
        ]});
        assert_eq!(asset_digest(&release, "buildx-v1.darwin-arm64").as_deref(), Some("6f20ba"));
        assert_eq!(asset_digest(&release, "missing"), None);
    }

    #[test]
    fn dangling_links() {
        let dir = std::env::temp_dir().join(format!("berth-cli-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("l");
        std::os::unix::fs::symlink(dir.join("missing"), &link).unwrap();
        assert!(is_dangling(&link));
        fs::write(dir.join("missing"), "").unwrap();
        assert!(!is_dangling(&link));
        fs::remove_dir_all(dir).ok();
    }
}
