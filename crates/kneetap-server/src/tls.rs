use std::fmt::{self, Debug, Formatter};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kneetap_api::{CertificateSource, ClientVerification, TlsReport};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, ring, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, RootCertStore, ServerConfig,
    ServerConnection, SignatureScheme,
};

use crate::Error;

/// The certificate a server presents and how it treats client certificates.
///
/// Only paths are held here. Key material is read when the server is built and then lives inside
/// rustls, so it can't reach logs through this type.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    files: Option<(PathBuf, PathBuf)>,
    client_ca: Option<PathBuf>,
}

impl TlsConfig {
    /// Uses a self-signed certificate generated at startup, which clients must choose to trust.
    #[must_use]
    pub const fn generated() -> Self {
        Self {
            files: None,
            client_ca: None,
        }
    }

    /// Uses a PEM certificate chain and private key.
    #[must_use]
    pub fn files(certificate: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
        Self {
            files: Some((certificate.into(), key.into())),
            client_ca: None,
        }
    }

    /// Requires clients to present a certificate signed by an authority in this PEM bundle.
    ///
    /// Without this, a client certificate is optional and reported as unverified.
    #[must_use]
    pub fn client_ca(mut self, path: impl Into<PathBuf>) -> Self {
        self.client_ca = Some(path.into());
        self
    }
}

impl TlsConfig {
    /// Identifies the current version of each file, to notice when one changes.
    ///
    /// Hashes contents because a replacement can keep the size and modification time, and a missed
    /// authority change would keep trusting clients it should refuse. Reading follows symbolic
    /// links, so this sees the atomic link swaps Kubernetes uses. Only hashes are kept, so key
    /// material stays inside rustls.
    pub(crate) fn file_versions(&self) -> Vec<Option<u64>> {
        let (certificate, key) = self.files.as_ref().map(|(c, k)| (c, k)).unzip();
        [certificate, key, self.client_ca.as_ref()]
            .into_iter()
            .flatten()
            .map(|path| {
                let mut hasher = DefaultHasher::new();
                fs::read(path).ok()?.hash(&mut hasher);
                Some(hasher.finish())
            })
            .collect()
    }
}

/// Validated TLS state, ready to accept connections.
pub struct Material {
    pub config: Arc<ServerConfig>,
    source: CertificateSource,
    requires_client_ca: bool,

    /// A generated identity, kept so that a reload doesn't change it. It has no file to reread.
    generated: Option<Identity>,
}

impl Debug for Material {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Material")
            .field("source", &self.source)
            .field("requires_client_ca", &self.requires_client_ca)
            .finish_non_exhaustive()
    }
}

impl Material {
    /// Loads and validates everything a handshake needs.
    pub fn load(tls: &TlsConfig) -> Result<Self, Error> {
        Self::build(tls, None)
    }

    /// Builds a replacement, which is complete and valid before anything uses it.
    pub fn reload(&self, tls: &TlsConfig) -> Result<Self, Error> {
        Self::build(tls, self.generated.as_ref())
    }

    /// Builds the configuration, reusing `generated` rather than making a new identity.
    ///
    /// The client policy is built first, so that no fallback for the server's own certificate can
    /// run before a broken authority has failed closed.
    fn build(tls: &TlsConfig, generated: Option<&Identity>) -> Result<Self, Error> {
        let verifier = client_verifier(tls.client_ca.as_deref())?;
        let identity = match (&tls.files, generated) {
            (Some((certificate, key)), _) => read_identity(certificate, key),
            (None, Some((chain, key))) => Ok((chain.clone(), key.clone_key())),
            (None, None) => generate_identity(),
        };
        let (chain, key) = identity.map_err(Error::tls_identity)?;
        let (source, generated) = if tls.files.is_some() {
            (CertificateSource::File, None)
        } else {
            let identity = (chain.clone(), key.clone_key());
            (CertificateSource::Generated, Some(identity))
        };
        let mut config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(Error::tls_identity)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .map_err(Error::tls_identity)?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Self {
            config: Arc::new(config),
            source,
            requires_client_ca: tls.client_ca.is_some(),
            generated,
        })
    }

    /// Summarizes a completed handshake for reports.
    pub fn describe(&self, connection: &ServerConnection) -> TlsReport {
        let certificates = connection.peer_certificates().unwrap_or_default();
        let client_verification = match (certificates.is_empty(), self.requires_client_ca) {
            (true, _) => ClientVerification::Absent,
            (false, true) => ClientVerification::Verified,
            (false, false) => ClientVerification::Unverified,
        };
        TlsReport {
            version: connection
                .protocol_version()
                .map(|version| format!("{version:?}"))
                .unwrap_or_default(),
            cipher: connection
                .negotiated_cipher_suite()
                .map(|cipher| format!("{:?}", cipher.suite()))
                .unwrap_or_default(),
            alpn: connection
                .alpn_protocol()
                .map(|alpn| String::from_utf8_lossy(alpn).into_owned()),
            certificate_source: self.source,
            client_verification,
            client_subjects: certificates.iter().filter_map(subject).collect(),
        }
    }
}

/// Formats a certificate's subject, or `None` if it can't be parsed.
fn subject(certificate: &CertificateDer<'_>) -> Option<String> {
    let (_, parsed) = x509_parser::parse_x509_certificate(certificate).ok()?;
    Some(parsed.subject().to_string())
}

/// Passes the provider explicitly, because a library must not set process-wide defaults.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Requires certificates from the authorities in `path`, or inspects any client's without one.
fn client_verifier(path: Option<&Path>) -> Result<Arc<dyn ClientCertVerifier>, Error> {
    let Some(path) = path else {
        return Ok(Arc::new(InspectOnly));
    };
    let bundle = fs::read(path).map_err(Error::client_ca)?;
    let mut roots = RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(&bundle) {
        let certificate = certificate.map_err(Error::client_ca)?;
        roots.add(certificate).map_err(Error::client_ca)?;
    }
    if roots.is_empty() {
        return Err(Error::client_ca("the bundle has no certificates"));
    }
    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
        .build()
        .map_err(Error::client_ca)
}

type Identity = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

type IdentityError = Box<dyn std::error::Error + Send + Sync>;

fn read_identity(certificate: &Path, key: &Path) -> Result<Identity, IdentityError> {
    let chain = fs::read(certificate)?;
    let chain = CertificateDer::pem_slice_iter(&chain).collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(&fs::read(key)?)?;
    Ok((chain, key))
}

/// Generates a self-signed certificate that names only the loopback addresses.
fn generate_identity() -> Result<Identity, IdentityError> {
    let names = ["localhost", "127.0.0.1", "::1"].map(str::to_owned);
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(names.to_vec())?;
    let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der());
    Ok((vec![cert.der().clone()], key.into()))
}

/// Accepts any well-formed client certificate so that its subject can be reported.
///
/// This establishes no trust in the certificate, but signatures are still checked, which proves the
/// client holds the matching key.
#[derive(Debug)]
struct InspectOnly;

impl ClientCertVerifier for InspectOnly {
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        certificate: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        x509_parser::parse_x509_certificate(certificate)
            .map_err(|_| CertificateError::BadEncoding)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = ring::default_provider().signature_verification_algorithms;
        verify_tls12_signature(message, certificate, signature, &algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = ring::default_provider().signature_verification_algorithms;
        verify_tls13_signature(message, certificate, signature, &algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn a_broken_client_authority_is_not_an_identity_error() {
        let tls = TlsConfig::generated().client_ca("/missing/ca.pem");

        let error = Material::load(&tls).unwrap_err();

        assert!(!error.is_tls_identity(), "{error}: {:?}", error.source());
    }

    #[test]
    fn a_client_authority_bundle_without_certificates_stops_startup() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let tls = TlsConfig::generated().client_ca(file.path());

        let error = Material::load(&tls).unwrap_err();

        assert_eq!(
            error.source().unwrap().to_string(),
            "the bundle has no certificates"
        );
    }

    #[test]
    fn a_missing_certificate_is_an_identity_error() {
        let tls = TlsConfig::files("/missing/cert.pem", "/missing/key.pem");

        let error = Material::load(&tls).unwrap_err();

        assert!(error.is_tls_identity(), "{error}");
    }

    #[test]
    fn a_replacement_with_the_same_size_and_time_is_a_new_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ca.pem");
        fs::write(&path, "first").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let tls = TlsConfig::generated().client_ca(&path);
        let before = tls.file_versions();

        fs::write(&path, "other").unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(modified).unwrap();

        assert_ne!(tls.file_versions(), before);
    }

    #[test]
    fn debug_output_never_contains_key_material() {
        let material = Material::load(&TlsConfig::generated()).unwrap();

        assert_eq!(
            format!("{material:?}"),
            "Material { source: Generated, requires_client_ca: false, .. }"
        );
    }
}
