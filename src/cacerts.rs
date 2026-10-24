//! CA certificates the Mac trusts beyond Apple's built-in roots (a company's
//! root installed by MDM, a TLS-inspecting proxy's, a private registry's),
//! for the VM: Docker's pulls and pushes and apt verify against them. Written
//! to ~/.berth/guest/ca-certificates.pem on each start; the VM installs them
//! into Debian's trust store before Docker starts (agent/src/main.rs), so a
//! change applies at the next start. Containers don't get them.
//!
//! Only CA certificates, from the System and login keychains, that macOS
//! itself says are trusted (`security verify-cert`): a certificate sitting
//! in a keychain isn't necessarily trusted.

use std::path::Path;

use crate::paths::Paths;

pub fn export(paths: &Paths) {
    let keychains = [Path::new("/Library/Keychains/System.keychain").to_path_buf(), paths.home.join("Library/Keychains/login.keychain-db")];
    // A process per certificate adds up: skip it all when neither keychain
    // (nor its trust settings, which macOS keeps beside them) has changed.
    let stamp = keychains
        .iter()
        .chain([Path::new("/Library/Trust Settings/Admin.plist").to_path_buf(), paths.home.join("Library/Trust Settings/User.plist")].iter())
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).map_or(0, |t| t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())))
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let stamp_path = paths.guest().join("ca-certificates.stamp");
    if paths.guest().join("ca-certificates.pem").exists() && std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str()) {
        return;
    }
    let mut bundle = String::new();
    let tmp = paths.guest().join("ca-check.pem");
    for keychain in keychains {
        let Ok(out) = std::process::Command::new("/usr/bin/security").args(["find-certificate", "-a", "-p"]).arg(&keychain).output() else {
            continue;
        };
        for pem in pems(&String::from_utf8_lossy(&out.stdout)) {
            if std::fs::write(&tmp, &pem).is_ok() && is_ca(&tmp) && trusted(&tmp) && !bundle.contains(&pem) {
                bundle.push_str(&pem);
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    let path = paths.guest().join("ca-certificates.pem");
    if std::fs::read_to_string(&path).unwrap_or_default() != bundle {
        match std::fs::write(&path, &bundle) {
            Ok(()) => eprintln!("cacerts: {} CA certificates from the Mac's keychains for the VM", bundle.matches("BEGIN CERTIFICATE").count()),
            Err(err) => {
                eprintln!("cacerts: {}: {err}", path.display());
                return;
            }
        }
    }
    let _ = std::fs::write(stamp_path, stamp);
}

/// The PEM blocks in `text`.
fn pems(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.starts_with("-----BEGIN CERTIFICATE-----") {
            current = Some(String::new());
        }
        if let Some(pem) = current.as_mut() {
            pem.push_str(line);
            pem.push('\n');
            if line.starts_with("-----END CERTIFICATE-----") {
                out.extend(current.take());
            }
        }
    }
    out
}

fn is_ca(pem: &Path) -> bool {
    std::process::Command::new("/usr/bin/openssl")
        .args(["x509", "-noout", "-text", "-in"])
        .arg(pem)
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("CA:TRUE"))
}

/// Trusted for TLS by macOS (a CA's own certificate verifies only if it's a
/// trusted root, or chains to one).
fn trusted(pem: &Path) -> bool {
    std::process::Command::new("/usr/bin/security")
        .args(["verify-cert", "-q", "-p", "ssl", "-c"])
        .arg(pem)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pem_blocks() {
        let text = "junk\n-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\nmore\n-----BEGIN CERTIFICATE-----\nBBB\n-----END CERTIFICATE-----\n";
        let blocks = pems(text);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[1].contains("BBB") && blocks[1].ends_with("-----END CERTIFICATE-----\n"));
    }
}
