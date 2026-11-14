//! The TLS-terminating proxy's CA (sandbox-runtime's `QS`, `nE`, `c5`), the
//! leaf certificates it mints per host (`F5`) and the trust bundle
//! sandboxed commands are pointed at (`a5`).

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PublicKeyData as _, RsaKeySize, SanType, SerialNumber,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use time::{Duration, OffsetDateTime};

/// Leaves and server configurations kept per CA (`MC`).
const CACHE_LIMIT: usize = 256;
/// The ephemeral CA's lifetime in days.
const CA_DAYS: i64 = 825;
/// A leaf's lifetime cap in days.
const LEAF_DAYS: i64 = 99;

/// One CA: the ephemeral one sandbox-runtime makes when no paths are set,
/// or the configured pair.
pub(crate) struct Ca {
    issuer: Issuer<'static, KeyPair>,
    cert_der: CertificateDer<'static>,
    not_after: OffsetDateTime,
    /// One key for every leaf, made on first use.
    leaf_key: OnceLock<KeyPair>,
    configs: Mutex<VecDeque<(String, Arc<ServerConfig>)>>,
    /// The CA certificate as commands see it.
    pub cert_path: PathBuf,
    /// The CA plus the default roots: what the trust variables point at.
    pub bundle_path: PathBuf,
    /// Folders removed with the CA (the bundle's always, the CA's own only
    /// when it is ephemeral).
    cleanup: Vec<PathBuf>,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ca").field("cert_path", &self.cert_path).field("bundle_path", &self.bundle_path).finish()
    }
}

impl Drop for Ca {
    fn drop(&mut self) {
        for dir in &self.cleanup {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// 16 random bytes, the first nibble 4–7 (`rE`): positive, never short.
fn serial() -> SerialNumber {
    let mut bytes = *uuid::Uuid::new_v4().as_bytes();
    bytes[0] = (bytes[0] & 0x3f) | 0x40;
    SerialNumber::from_slice(&bytes)
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// `mkdtemp(<dir>/srt-ca-XXXXXX)`, mode 0700.
fn temp_dir(base: &Path) -> io::Result<PathBuf> {
    for _ in 0..16 {
        let name: String = std::iter::repeat_with(fastrand::alphanumeric).take(6).collect();
        let dir = base.join(format!("srt-ca-{name}"));
        match fs::create_dir(&dir) {
            Ok(()) => {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                return Ok(dir);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free srt-ca folder name"))
}

fn write(path: &Path, text: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write as _;
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(path)?;
    f.write_all(text)
}

fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out + &format!("-----END {label}-----\n")
}

fn err(text: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, text)
}

impl Ca {
    /// The CA of `sandbox.network.tlsTerminate`: the pair at `cert`/`key`,
    /// or a new ephemeral one written under `tmp`.
    pub(crate) fn new(paths: Option<(&Path, &Path)>, tmp: &Path) -> io::Result<Self> {
        let (issuer, cert_der, cert_pem, not_after, cert_path, mut cleanup) = match paths {
            Some((cert, key)) => {
                let (issuer, der, pem, not_after) = load(cert, key)?;
                (issuer, der, pem, not_after, cert.to_path_buf(), Vec::new())
            }
            None => {
                let (issuer, der, pem, not_after) = ephemeral()?;
                let dir = temp_dir(tmp)?;
                let cert_path = dir.join("ca.crt");
                write(&cert_path, pem.as_bytes(), 0o644)?;
                write(&dir.join("ca.key"), issuer.key().serialize_pem().as_bytes(), 0o600)?;
                tracing::debug!("[mitm-ca] generated ephemeral CA at {}", cert_path.display());
                (issuer, der, pem, not_after, cert_path, vec![dir])
            }
        };
        let bundle_dir = temp_dir(tmp)?;
        let bundle_path = bundle_dir.join("trust-bundle.crt");
        write(&bundle_path, trust_bundle(&cert_pem).as_bytes(), 0o644)?;
        cleanup.push(bundle_dir);
        Ok(Self {
            issuer,
            cert_der,
            not_after,
            leaf_key: OnceLock::new(),
            configs: Mutex::default(),
            cert_path,
            bundle_path,
            cleanup,
        })
    }

    /// The server side of a terminated tunnel for `name` (SNI or the
    /// CONNECT host): a leaf for it, the CA after it, `http/1.1` only.
    pub(crate) fn server_config(&self, name: &str) -> io::Result<Arc<ServerConfig>> {
        let mut configs = self.configs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = configs.iter().position(|(n, _)| n == name) {
            let hit = configs.remove(at).expect("present");
            let config = Arc::clone(&hit.1);
            configs.push_back(hit);
            return Ok(config);
        }
        drop(configs);
        let leaf = self.leaf(name)?;
        let key = self.leaf_key()?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| err(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(vec![leaf, self.cert_der.clone()], key)
            .map_err(|e| err(e.to_string()))?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        let mut configs = self.configs.lock().unwrap_or_else(PoisonError::into_inner);
        configs.push_back((name.to_owned(), Arc::clone(&config)));
        while configs.len() > CACHE_LIMIT {
            configs.pop_front();
        }
        Ok(config)
    }

    fn leaf_key(&self) -> io::Result<&KeyPair> {
        if let Some(key) = self.leaf_key.get() {
            return Ok(key);
        }
        let key =
            KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, RsaKeySize::_2048).map_err(|e| err(e.to_string()))?;
        Ok(self.leaf_key.get_or_init(|| key))
    }

    /// `F5`: CN and one SAN (an address or a DNS name), from a day ago to 99
    /// days on (never past the CA), server auth only.
    fn leaf(&self, name: &str) -> io::Result<CertificateDer<'static>> {
        let mut params = CertificateParams::default();
        params.serial_number = Some(serial());
        params.not_before = now() - Duration::days(1);
        params.not_after = (params.not_before + Duration::days(LEAF_DAYS)).min(self.not_after);
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, name);
        params.distinguished_name = dn;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.subject_alt_names = vec![match name.parse::<IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(name.try_into().map_err(|e: rcgen::Error| err(e.to_string()))?),
        }];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(self.leaf_key()?, &self.issuer).map_err(|e| err(e.to_string()))?;
        Ok(cert.der().clone())
    }

    /// The CA certificate (DER).
    #[cfg(test)]
    pub(crate) fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }
}

/// `nE`: RSA-2048, `CN=sandbox-runtime ephemeral CA, O=sandbox-runtime`,
/// from a day ago for 825 days, CA, `keyCertSign cRLSign digitalSignature`.
fn ephemeral() -> io::Result<(Issuer<'static, KeyPair>, CertificateDer<'static>, String, OffsetDateTime)> {
    let key = KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, RsaKeySize::_2048).map_err(|e| err(e.to_string()))?;
    let mut params = CertificateParams::default();
    params.serial_number = Some(serial());
    params.not_before = now() - Duration::days(1);
    params.not_after = now() + Duration::days(CA_DAYS);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "sandbox-runtime ephemeral CA");
    dn.push(DnType::OrganizationName, "sandbox-runtime");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    let not_after = params.not_after;
    let cert = params.self_signed(&key).map_err(|e| err(e.to_string()))?;
    let der = cert.der().clone();
    let pem = cert.pem();
    Ok((Issuer::new(params, key), der, pem, not_after))
}

/// `c5`, `Cm`: the configured pair, checked as sandbox-runtime checks it.
fn load(
    cert: &Path,
    key: &Path,
) -> io::Result<(Issuer<'static, KeyPair>, CertificateDer<'static>, String, OffsetDateTime)> {
    let read = |path: &Path, setting: &str, label: &str| -> io::Result<String> {
        let text = fs::read_to_string(path).map_err(|e| {
            let code =
                e.raw_os_error().map_or_else(|| e.kind().to_string(), |c| io::Error::from_raw_os_error(c).to_string());
            err(format!("tlsTerminate.{setting}: cannot read {} ({code})", path.display()))
        })?;
        let header = regex::Regex::new(&format!("-----BEGIN [A-Z ]*{label}-----")).map_err(|e| err(e.to_string()))?;
        if !header.is_match(&text) {
            return Err(err(format!("tlsTerminate.{setting}: {} is not a PEM {label}", path.display())));
        }
        Ok(text)
    };
    let cert_pem = read(cert, "caCertPath", "CERTIFICATE")?;
    let key_pem = read(key, "caKeyPath", "PRIVATE KEY")?;
    let fail = |reason: String| err(format!("tlsTerminate: CA at {}: {reason}", cert.display()));
    let key = KeyPair::from_pem(&key_pem).map_err(|e| fail(format!("parse: {e}")))?;
    if key.algorithm() != &rcgen::PKCS_RSA_SHA256 {
        return Err(fail("key is not RSA".into()));
    }
    let block = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).map_err(|e| fail(format!("parse: {e}")))?.1;
    let x509 = block.parse_x509().map_err(|e| fail(format!("parse: {e}")))?;
    let not_before = x509.validity().not_before.to_datetime();
    if not_before > now() {
        return Err(fail(format!("notBefore {not_before} is in the future (clock skew?)")));
    }
    if x509.public_key().raw != key.subject_public_key_info().as_slice() {
        return Err(fail("key does not match certificate public key".into()));
    }
    let not_after = x509.validity().not_after.to_datetime();
    let der = CertificateDer::from(block.contents.clone());
    let issuer = Issuer::from_ca_cert_der(&der, key).map_err(|e| fail(format!("parse: {e}")))?;
    let pem_text = pem("CERTIFICATE", &block.contents);
    Ok((issuer, der, pem_text, not_after))
}

/// `a5`: the CA, the default roots (Mozilla's, as Node's
/// `tls.rootCertificates`), and the file `NODE_EXTRA_CA_CERTS` names.
fn trust_bundle(ca_pem: &str) -> String {
    let mut parts = vec![ca_pem.trim().to_owned()];
    for root in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
        parts.push(pem("CERTIFICATE", root.as_ref()).trim().to_owned());
    }
    if let Some(extra) = std::env::var_os("NODE_EXTRA_CA_CERTS").filter(|v| !v.is_empty())
        && let Ok(text) = fs::read_to_string(extra)
    {
        parts.push(text.trim().to_owned());
    }
    parts.join("\n") + "\n"
}

/// What the upstream side trusts: the default roots and the file
/// `NODE_EXTRA_CA_CERTS` names (Node reads it into its default store).
pub(crate) fn upstream_roots() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    for root in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
        let _ = roots.add(root.clone());
    }
    if let Some(extra) = std::env::var_os("NODE_EXTRA_CA_CERTS").filter(|v| !v.is_empty())
        && let Ok(text) = fs::read(extra)
    {
        for block in x509_parser::pem::Pem::iter_from_buffer(&text).flatten() {
            if block.label == "CERTIFICATE" {
                let _ = roots.add(CertificateDer::from(block.contents));
            }
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let dir = fs::canonicalize(std::env::temp_dir()).expect("tmp").join(format!("eos-ca-{}", fastrand::u64(..)));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn an_ephemeral_ca_and_its_leaves() {
        let base = tmp();
        let ca = Ca::new(None, &base).expect("ca");
        assert!(ca.cert_path.ends_with("ca.crt") && ca.cert_path.parent().expect("dir").join("ca.key").exists());
        let mode = fs::metadata(ca.cert_path.parent().expect("dir").join("ca.key")).expect("key").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let bundle = fs::read_to_string(&ca.bundle_path).expect("bundle");
        assert!(bundle.matches("-----BEGIN CERTIFICATE-----").count() > 100, "the CA and the default roots");
        let (_, x509) = x509_parser::parse_x509_certificate(ca.cert_der()).expect("ca");
        assert_eq!(x509.subject().to_string(), "CN=sandbox-runtime ephemeral CA, O=sandbox-runtime");
        assert!(x509.is_ca());

        let leaf = ca.leaf("api.example.com").expect("leaf");
        let (_, leaf) = x509_parser::parse_x509_certificate(&leaf).expect("leaf");
        assert_eq!(leaf.issuer(), x509.subject());
        let days = (leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp()) / 86_400;
        assert_eq!(days, 99);
        let ip = ca.leaf("127.0.0.1").expect("ip leaf");
        let (_, ip) = x509_parser::parse_x509_certificate(&ip).expect("ip");
        let san = ip.subject_alternative_name().expect("ext").expect("san");
        assert!(matches!(san.value.general_names[0], x509_parser::extensions::GeneralName::IPAddress(_)));
        let a = ca.server_config("a.com").expect("config");
        assert!(Arc::ptr_eq(&a, &ca.server_config("a.com").expect("again")), "cached");

        let dirs = ca.cleanup.clone();
        drop(ca);
        assert!(dirs.iter().all(|d| !d.exists()), "removed with the CA");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_configured_pair_is_checked() {
        let base = tmp();
        let ca = Ca::new(None, &base).expect("ca");
        let dir = ca.cert_path.parent().expect("dir").to_path_buf();
        let loaded = Ca::new(Some((&ca.cert_path, &dir.join("ca.key"))), &base).expect("loaded");
        assert_eq!(loaded.cert_path, ca.cert_path, "used where it is");
        let leaf = loaded.leaf("x.com").expect("leaf");
        let (_, leaf) = x509_parser::parse_x509_certificate(&leaf).expect("leaf");
        assert_eq!(leaf.issuer().to_string(), "CN=sandbox-runtime ephemeral CA, O=sandbox-runtime");

        let other = Ca::new(None, &base).expect("other");
        let mismatch = Ca::new(Some((&ca.cert_path, &other.cert_path.parent().expect("d").join("ca.key"))), &base);
        assert!(mismatch.expect_err("mismatch").to_string().ends_with("key does not match certificate public key"));
        let not_pem = Ca::new(Some((&dir.join("ca.key"), &dir.join("ca.key"))), &base).expect_err("not a cert");
        assert!(not_pem.to_string().contains("is not a PEM CERTIFICATE"), "{not_pem}");
        let missing = Ca::new(Some((&dir.join("nope"), &dir.join("ca.key"))), &base).expect_err("missing");
        assert!(missing.to_string().starts_with("tlsTerminate.caCertPath: cannot read"), "{missing}");
        drop((ca, loaded, other));
        let _ = fs::remove_dir_all(&base);
    }
}
