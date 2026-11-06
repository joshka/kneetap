use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::WantsClientCert;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ConfigBuilder, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::Error;

/// How a client decides whether to trust a server's certificate.
#[derive(Debug, Default)]
pub enum Trust {
    /// The built-in public roots.
    #[default]
    PublicRoots,

    /// Only the authorities in this PEM bundle.
    Bundle(PathBuf),

    /// Any certificate, so the server's identity isn't checked.
    Any,
}

/// Builds the TLS configuration, passing the provider explicitly so no process-wide default is set.
///
/// The default roots are compiled in because the `scratch` image has no system certificates.
pub fn config(trust: &Trust, identity: Option<&(PathBuf, PathBuf)>) -> Result<ClientConfig, Error> {
    let provider = Arc::new(ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(Error::tls)?;
    let builder = match trust {
        Trust::PublicRoots => builder.with_root_certificates(public_roots()),
        Trust::Bundle(path) => builder.with_root_certificates(bundle_roots(path)?),
        Trust::Any => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(provider))),
    };
    let mut config = match identity {
        Some((certificate, key)) => with_identity(builder, certificate, key)?,
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// Finishes `builder` with a client certificate read from PEM files.
fn with_identity(
    builder: ConfigBuilder<ClientConfig, WantsClientCert>,
    certificate: &Path,
    key: &Path,
) -> Result<ClientConfig, Error> {
    let chain = fs::read(certificate).map_err(Error::tls)?;
    let chain = CertificateDer::pem_slice_iter(&chain).collect::<Result<Vec<_>, _>>();
    let chain = chain.map_err(Error::tls)?;
    let key = fs::read(key).map_err(Error::tls)?;
    let key = PrivateKeyDer::from_pem_slice(&key).map_err(Error::tls)?;
    let config = builder.with_client_auth_cert(chain, key);
    config.map_err(Error::tls)
}

/// Returns the compiled-in public roots.
fn public_roots() -> RootCertStore {
    RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

/// Loads the authorities in `path`.
///
/// An empty bundle is an error, because it could never verify a server.
fn bundle_roots(path: &Path) -> Result<RootCertStore, Error> {
    let bundle = fs::read(path).map_err(Error::tls)?;
    let mut store = RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(&bundle) {
        let certificate = certificate.map_err(Error::tls)?;
        store.add(certificate).map_err(Error::tls)?;
    }
    if store.is_empty() {
        return Err(Error::tls("the root bundle has no certificates"));
    }
    Ok(store)
}

/// Skips checking who the server is, while still verifying handshake signatures.
#[derive(Debug)]
struct AcceptAny(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.0.signature_verification_algorithms;
        verify_tls12_signature(message, certificate, signature, algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.0.signature_verification_algorithms;
        verify_tls13_signature(message, certificate, signature, algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
