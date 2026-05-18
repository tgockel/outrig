//! Per-session MITM CA and leaf-cert minting.
//!
//! When MITM is enabled, the interceptor mints a fresh ECDSA P-256 CA at
//! startup, writes its public cert plus private key into `<session_dir>/tls/`
//! (key permissions 0600), and issues per-host leaf certificates on demand
//! from a small FIFO-bounded cache. Nothing here touches the host trust
//! anchors; the CA lives only for the session.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType,
};
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::error::{OutrigError, Result};

/// Validity window for the session CA and every leaf cert it signs. Long
/// enough to cover overnight benchmarks; short enough that a leaked
/// `ca.key` doesn't outlive a typical machine reimage.
const CA_VALIDITY_DAYS: i64 = 30;

/// Cap on the leaf-cert cache. Bounded to guard against SNI flooding from
/// inside the container; eviction is FIFO when the cap is exceeded.
const LEAF_CACHE_CAP: usize = 256;

/// Filenames used by the on-disk CA layout. The private key sits inside
/// the session directory at mode 0600.
pub const CA_CERT_FILE: &str = "ca.crt";
pub const CA_KEY_FILE: &str = "ca.key";
pub const CA_DIR: &str = "tls";

/// One session-scoped CA plus the leaf-cert cache backing the MITM TLS
/// handshakes. Cloning is cheap (Arc internally).
#[derive(Clone)]
pub struct SessionCa {
    inner: Arc<SessionCaInner>,
}

struct SessionCaInner {
    /// Signed CA certificate (PEM form is also written to disk).
    ca_cert: Certificate,
    /// CA signing key; never leaves this process.
    ca_key: KeyPair,
    /// Path to the on-disk private key, used by `cleanup` to wipe it.
    ca_key_path: PathBuf,
    leaf_cache: Mutex<LeafCache>,
}

struct LeafCache {
    map: BTreeMap<String, Arc<CertifiedKey>>,
    order: VecDeque<String>,
}

impl LeafCache {
    fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &str) -> Option<Arc<CertifiedKey>> {
        self.map.get(key).cloned()
    }

    fn insert(&mut self, key: String, value: Arc<CertifiedKey>) {
        if self.map.contains_key(&key) {
            return;
        }
        while self.map.len() >= LEAF_CACHE_CAP {
            if let Some(victim) = self.order.pop_front() {
                self.map.remove(&victim);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

impl SessionCa {
    /// Generate a fresh CA, write the cert (mode 0644) and the key
    /// (mode 0600) to `<session_dir>/tls/`, and return a handle that can
    /// mint leaf certs on demand.
    pub fn generate(session_dir: &Path, session_id: &str) -> Result<Self> {
        let ca_dir = session_dir.join(CA_DIR);
        std::fs::create_dir_all(&ca_dir).map_err(|e| {
            OutrigError::Configuration(format!("creating MITM ca dir {}: {e}", ca_dir.display()))
        })?;

        let (ca_cert, ca_key) = mint_ca(session_id)?;

        let ca_cert_path = ca_dir.join(CA_CERT_FILE);
        let ca_key_path = ca_dir.join(CA_KEY_FILE);
        write_file_mode(&ca_cert_path, ca_cert.pem().as_bytes(), 0o644)?;
        write_file_mode(&ca_key_path, ca_key.serialize_pem().as_bytes(), 0o600)?;

        Ok(Self {
            inner: Arc::new(SessionCaInner {
                ca_cert,
                ca_key,
                ca_key_path,
                leaf_cache: Mutex::new(LeafCache::new()),
            }),
        })
    }

    /// PEM-encoded CA certificate. Suitable for installing into the
    /// container trust store.
    pub fn ca_pem(&self) -> String {
        self.inner.ca_cert.pem()
    }

    /// Mint (or fetch from cache) a leaf certificate keyed on the given
    /// server name. The returned `CertifiedKey` is ready to hand to a
    /// rustls `ResolvesServerCert` implementation.
    pub fn leaf_for(&self, server_name: &str) -> Result<Arc<CertifiedKey>> {
        let key = server_name.to_ascii_lowercase();
        if let Some(found) = self.inner.leaf_cache.lock().unwrap().get(&key) {
            return Ok(found);
        }
        let certified = mint_leaf(&self.inner.ca_cert, &self.inner.ca_key, &key)?;
        let arc = Arc::new(certified);
        self.inner
            .leaf_cache
            .lock()
            .unwrap()
            .insert(key, arc.clone());
        Ok(arc)
    }

    /// Best-effort removal of the on-disk private key. The public cert is
    /// left in place so audit-log readers can verify recorded handshakes
    /// after the session ends.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.inner.ca_key_path);
    }

    /// SNI-driven resolver suitable for plugging into a
    /// `rustls::ServerConfig`.
    pub fn resolver(&self) -> Arc<dyn ResolvesServerCert> {
        Arc::new(CaResolver { ca: self.clone() })
    }
}

struct CaResolver {
    ca: SessionCa,
}

impl std::fmt::Debug for CaResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaResolver").finish()
    }
}

impl ResolvesServerCert for CaResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = client_hello.server_name()?;
        self.ca.leaf_for(sni).ok()
    }
}

fn mint_ca(session_id: &str) -> Result<(Certificate, KeyPair)> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(rcgen_err)?;

    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(rcgen_err)?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "outrig");
    dn.push(
        DnType::CommonName,
        format!("outrig session {session_id} CA"),
    );
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let (nb, na) = validity_window();
    params.not_before = nb;
    params.not_after = na;

    let cert = params.self_signed(&key).map_err(rcgen_err)?;
    Ok((cert, key))
}

fn mint_leaf(ca_cert: &Certificate, ca_key: &KeyPair, server_name: &str) -> Result<CertifiedKey> {
    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(rcgen_err)?;

    let mut params = CertificateParams::new(vec![server_name.to_string()]).map_err(rcgen_err)?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, server_name);
    params.distinguished_name = dn;
    params.subject_alt_names =
        vec![SanType::DnsName(server_name.try_into().map_err(|e| {
            OutrigError::Configuration(format!("invalid SNI {server_name:?}: {e}"))
        })?)];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let (nb, na) = validity_window();
    params.not_before = nb;
    params.not_after = na;

    let leaf_cert = params
        .signed_by(&leaf_key, ca_cert, ca_key)
        .map_err(rcgen_err)?;

    let key_der = PrivatePkcs8KeyDer::from(leaf_key.serialize_der());
    let signing_key = any_supported_type(&PrivateKeyDer::Pkcs8(key_der))
        .map_err(|e| OutrigError::Configuration(format!("loading MITM leaf key: {e:?}")))?;
    let cert_chain = vec![
        CertificateDer::from(leaf_cert.der().to_vec()),
        CertificateDer::from(ca_cert.der().to_vec()),
    ];
    Ok(CertifiedKey::new(cert_chain, signing_key))
}

fn rcgen_err(e: rcgen::Error) -> OutrigError {
    OutrigError::Configuration(format!("MITM cert generation: {e}"))
}

fn validity_window() -> (OffsetDateTime, OffsetDateTime) {
    let now = OffsetDateTime::now_utc();
    let nb = now - TimeDuration::seconds(60);
    let na = now + TimeDuration::days(CA_VALIDITY_DAYS);
    (nb, na)
}

/// rustls 0.23 requires a `CryptoProvider` to be installed before any
/// builder method picks a default. The call is idempotent (the second
/// `install_default` returns `Err`, which we deliberately drop).
pub(crate) fn ensure_rustls_provider_installed() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn write_file_mode(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(mode)
        .open(path)
        .map_err(|e| {
            OutrigError::Configuration(format!("writing MITM file {}: {e}", path.display()))
        })?;
    file.write_all(contents).map_err(|e| {
        OutrigError::Configuration(format!("writing MITM file {}: {e}", path.display()))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn ca_files_have_expected_permissions() {
        ensure_rustls_provider_installed();
        let dir = tempfile::tempdir().unwrap();
        let ca = SessionCa::generate(dir.path(), "20260101T000000-0001").unwrap();

        let key_path = dir.path().join(CA_DIR).join(CA_KEY_FILE);
        let cert_path = dir.path().join(CA_DIR).join(CA_CERT_FILE);
        assert!(cert_path.exists());
        assert!(key_path.exists());

        let key_mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(key_mode, 0o600, "ca.key should be 0600, got {key_mode:o}");

        let pem = ca.ca_pem();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn leaf_cache_caps_at_capacity() {
        ensure_rustls_provider_installed();
        let dir = tempfile::tempdir().unwrap();
        let ca = SessionCa::generate(dir.path(), "20260101T000000-0002").unwrap();

        for n in 0..(LEAF_CACHE_CAP + 1) {
            let host = format!("h{n}.example.com");
            ca.leaf_for(&host).unwrap();
        }
        let len = ca.inner.leaf_cache.lock().unwrap().len();
        assert_eq!(len, LEAF_CACHE_CAP);
    }

    #[test]
    fn leaf_cache_returns_cached_handles() {
        ensure_rustls_provider_installed();
        let dir = tempfile::tempdir().unwrap();
        let ca = SessionCa::generate(dir.path(), "20260101T000000-0003").unwrap();
        let a = ca.leaf_for("example.com").unwrap();
        let b = ca.leaf_for("EXAMPLE.com").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "cache should be case-insensitive");
    }

    #[test]
    fn cleanup_removes_ca_key() {
        ensure_rustls_provider_installed();
        let dir = tempfile::tempdir().unwrap();
        let ca = SessionCa::generate(dir.path(), "20260101T000000-0004").unwrap();
        let key_path = dir.path().join(CA_DIR).join(CA_KEY_FILE);
        assert!(key_path.exists());
        ca.cleanup();
        assert!(!key_path.exists(), "cleanup should remove ca.key");
        assert!(
            dir.path().join(CA_DIR).join(CA_CERT_FILE).exists(),
            "cleanup must NOT remove ca.crt"
        );
    }
}
