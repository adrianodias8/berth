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

use anyhow::{Result, bail};

use crate::paths::Paths;

const DOCKER: &str = "29.9.2";
const COMPOSE: &str = "v5.6.1";
const BUILDX: &str = "v0.38.1";
/// Matches the k3s version in agent/src/k8s.rs.
const KUBECTL: &str = "v1.37.1";

// SHA-256 of each download, pinned with its version: a server serving
// something else (or a checksum file beside it changed) fails the download.
// Changing a version means updating its hash: `shasum -a 256` of the file,
// checked against what the project publishes.
const DOCKER_SHA256: &str = "ea8a2ce5ac31f9f5f3fb711a7a485e1e46cc7a69798a629a70306f6e6af25b7a";
const COMPOSE_SHA256: &str = "fcce41226abd2b7b2936094e688bbd61284673dddafea542e08cb9a3b4423a8b";
const BUILDX_SHA256: &str = "6f20ba2437865311ef99015bf5d1039c3c8e4d2177c1c64aa41b134b5ecd3012";
const KUBECTL_SHA256: &str = "fd65982c97ddad3106754b69ffa196d0e543aa591930ae52aed1adfb92f8c77f";

/// Names linked into /usr/local/bin by the helper (see helper.rs).
pub const SYSTEM_LINKS: [&str; 5] = ["docker", "docker-compose", "kubectl", "podman", "nerdctl"];

pub fn bin_dir(paths: &Paths) -> PathBuf {
    paths.root.join("bin")
}

/// Download anything missing or outdated, then link the CLI plugins.
pub fn ensure(paths: &Paths) -> Result<()> {
    let bin = bin_dir(paths);
    fs::create_dir_all(&bin)?;
    let stamp = bin.join(".versions");
    let want = format!("{DOCKER} {COMPOSE} {BUILDX} {KUBECTL}\n");
    if fs::read_to_string(&stamp).ok().as_deref() != Some(want.as_str()) {
        eprintln!("Downloading Docker CLI {DOCKER}, Compose {COMPOSE}, Buildx {BUILDX} and kubectl {KUBECTL}...");
        fetch_docker(&bin)?;
        fetch_verified_sha256(
            &bin.join("docker-compose"),
            &format!("https://github.com/docker/compose/releases/download/{COMPOSE}/docker-compose-darwin-aarch64"),
            COMPOSE_SHA256,
        )?;
        fetch_verified_sha256(
            &bin.join("docker-buildx"),
            &format!("https://github.com/docker/buildx/releases/download/{BUILDX}/buildx-{BUILDX}.darwin-arm64"),
            BUILDX_SHA256,
        )?;
        fetch_verified_sha256(
            &bin.join("kubectl"),
            &format!("https://dl.k8s.io/release/{KUBECTL}/bin/darwin/arm64/kubectl"),
            KUBECTL_SHA256,
        )?;
        fs::write(&stamp, want)?;
    }
    link_plugins(paths, &bin)
}

/// The CLI ships as a tarball: checked, unpacked into a folder of its own,
/// then the binary renamed into place (never a half-written `docker`).
fn fetch_docker(bin: &Path) -> Result<()> {
    let tmp = bin.join("docker.tgz");
    let unpack = bin.join(".docker-unpack");
    let _ = fs::remove_dir_all(&unpack);
    let result = (|| {
        fetch_verified_sha256(&tmp, &format!("https://download.docker.com/mac/static/stable/aarch64/docker-{DOCKER}.tgz"), DOCKER_SHA256)?;
        fs::create_dir(&unpack)?;
        let status = Command::new("tar").arg("-xzf").arg(&tmp).arg("-C").arg(&unpack).args(["--strip-components", "1", "docker/docker"]).status()?;
        if !status.success() {
            bail!("extracting the docker CLI failed");
        }
        fs::rename(unpack.join("docker"), bin.join("docker"))?;
        Ok(())
    })();
    fs::remove_file(&tmp).ok();
    fs::remove_dir_all(&unpack).ok();
    result
}

/// Download `url` to `dest` (executable), checking it against a known SHA-256.
pub fn fetch_verified_sha256(dest: &Path, url: &str, sha256: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = dest.with_extension("download");
    curl(url, &tmp)?;
    if !sha256_file(&tmp)?.eq_ignore_ascii_case(sha256) {
        fs::remove_file(&tmp).ok();
        bail!("checksum mismatch for {url}");
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, dest)?;
    Ok(())
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
