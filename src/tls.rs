//! HTTPS for container domains: a local CA and on-demand leaf certificates.
//!
//! The CA lives in ~/.berth/tls and is trusted once in the login keychain
//! (`berth tls trust`). It carries a name constraint limiting it to `.local`
//! names, and we only ever issue for `.local` names, so the trusted key can't
//! be used to impersonate real sites even if it leaked.

use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, NameConstraints,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use time::OffsetDateTime;

use crate::paths::Paths;

const CA_NAME: &str = "berth local CA";
/// Leaf lifetime; reissued well before expiry by the cache below.
const LEAF_DAYS: i64 = 30;
const LEAF_REISSUE: Duration = Duration::from_secs(20 * 24 * 3600);

pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    ca_der: CertificateDer<'static>,
    leaves: Mutex<HashMap<String, (Instant, Arc<CertifiedKey>)>>,
}

// rustls requires Debug on resolvers; rcgen's Issuer has none.
impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Authority")
    }
}

/// The CA's fixed identity. Rebuilt identically on every load: an Issuer only
/// needs the subject name and key, and the subject must match the stored cert.
fn ca_params() -> Result<CertificateParams> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name.push(DnType::CommonName, CA_NAME);
    params.distinguished_name.push(DnType::OrganizationName, "berth");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName("local".into())],
        excluded_subtrees: vec![],
    });
    Ok(params)
}

pub fn ca_cert_path(paths: &Paths) -> std::path::PathBuf {
    paths.root.join("tls/ca.crt")
}

impl Authority {
    pub fn load_or_create(paths: &Paths) -> Result<Self> {
        let dir = paths.root.join("tls");
        let (cert_path, key_path) = (ca_cert_path(paths), dir.join("ca.key"));
        if !cert_path.exists() || !key_path.exists() {
            fs::create_dir_all(&dir)?;
            let key = KeyPair::generate()?;
            let mut params = ca_params()?;
            let now = OffsetDateTime::now_utc();
            params.not_before = now - time::Duration::days(1);
            params.not_after = now + time::Duration::days(3650);
            let cert = params.self_signed(&key)?;
            write_private(&key_path, key.serialize_pem().as_bytes())?;
            fs::write(&cert_path, cert.pem())?;
            eprintln!("tls: created local CA at {}", cert_path.display());
        }
        let key = KeyPair::from_pem(&fs::read_to_string(&key_path)?).context("reading CA key")?;
        let ca_der = CertificateDer::from_pem_file(&cert_path).context("reading CA certificate")?;
        Ok(Self { issuer: Issuer::new(ca_params()?, key), ca_der, leaves: Mutex::new(HashMap::new()) })
    }

    pub fn acceptor(self: Arc<Self>) -> Result<tokio_rustls::TlsAcceptor> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(self);
        // We forward plain HTTP/1.1 to the container, so don't offer h2.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    }

    fn leaf(&self, name: &str) -> Result<Arc<CertifiedKey>> {
        if let Some((issued, key)) = self.leaves.lock().unwrap().get(name)
            && issued.elapsed() < LEAF_REISSUE
        {
            return Ok(key.clone());
        }
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![name.to_string()])?;
        params.distinguished_name.push(DnType::CommonName, name);
        let now = OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(LEAF_DAYS);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(&key, &self.issuer)?;
        let signing = rustls::crypto::ring::sign::any_ecdsa_type(&PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            key.serialize_der(),
        )))?;
        let certified = Arc::new(CertifiedKey::new(vec![cert.der().clone(), self.ca_der.clone()], signing));
        self.leaves.lock().unwrap().insert(name.to_string(), (Instant::now(), certified.clone()));
        Ok(certified)
    }
}

impl ResolvesServerCert for Authority {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.trim_end_matches('.').to_ascii_lowercase();
        if !is_local_name(&name) {
            return None;
        }
        self.leaf(&name).map_err(|err| eprintln!("tls: issuing for {name}: {err:#}")).ok()
    }
}

/// Only `<something>.local` names, the same restriction the CA itself carries.
fn is_local_name(name: &str) -> bool {
    name.len() > ".local".len()
        && name.ends_with(".local")
        && name.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'*'))
}

fn write_private(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(contents)?;
    Ok(())
}

// --- keychain trust, run from the CLI -------------------------------------

fn login_keychain(paths: &Paths) -> String {
    paths.home.join("Library/Keychains/login.keychain-db").to_string_lossy().into_owned()
}

/// Whether macOS currently trusts the CA for TLS.
pub fn is_trusted(paths: &Paths) -> bool {
    let cert = ca_cert_path(paths);
    cert.exists()
        && Command::new("security")
            .args(["verify-cert", "-q", "-p", "basic", "-c"])
            .arg(&cert)
            .status()
            .is_ok_and(|s| s.success())
}

pub fn trust(paths: &Paths) -> Result<()> {
    // Make sure the CA exists even if berth hasn't served HTTPS yet.
    Authority::load_or_create(paths)?;
    let cert = ca_cert_path(paths);
    println!("macOS will ask for your password to trust the berth CA (limited to .local names).");
    let status = Command::new("security")
        .args(["add-trusted-cert", "-r", "trustRoot", "-p", "ssl", "-k", &login_keychain(paths)])
        .arg(&cert)
        .status()?;
    if !status.success() {
        bail!("security add-trusted-cert failed ({status})");
    }
    println!("Trusted {}. Restart your browser if it was open.", cert.display());
    Ok(())
}

pub fn untrust(paths: &Paths) -> Result<()> {
    let cert = ca_cert_path(paths);
    let _ = Command::new("security").args(["remove-trusted-cert"]).arg(&cert).status();
    let _ = Command::new("security")
        .args(["delete-certificate", "-c", CA_NAME, &login_keychain(paths)])
        .status();
    println!("Removed trust for the berth CA.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_names() {
        assert!(is_local_name("web.berth.local"));
        assert!(is_local_name("foo.local"));
        assert!(!is_local_name("local"));
        assert!(!is_local_name(".local"));
        assert!(!is_local_name("google.com"));
        assert!(!is_local_name("evil.local.example.com"));
        assert!(!is_local_name("a..local"));
    }

    #[test]
    fn issues_chain_for_local_name() {
        let dir = std::env::temp_dir().join(format!("berth-tls-test-{}", std::process::id()));
        let paths = Paths { home: dir.clone(), root: dir.clone() };
        let ca = Authority::load_or_create(&paths).unwrap();
        let leaf = ca.leaf("web.berth.local").unwrap();
        assert_eq!(leaf.cert.len(), 2, "leaf + CA");
        // Reloading reuses the stored CA.
        let again = Authority::load_or_create(&paths).unwrap();
        assert_eq!(again.ca_der, ca.ca_der);
        fs::remove_dir_all(dir).ok();
    }
}
