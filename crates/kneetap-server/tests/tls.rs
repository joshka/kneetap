//! HTTPS and client certificates, exercised over real sockets.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hyper::Uri;
use hyper_util::rt::TokioIo;
use kneetap_api::{CertificateSource, ClientVerification, Report};
use kneetap_client::Client;
use kneetap_server::{Server, TlsConfig};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};
use rstest::rstest;
use rustls::crypto::ring;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tonic::transport::{Channel, Endpoint};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;

struct Files(TempDir);

impl Files {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}

type Authority = CertifiedIssuer<'static, KeyPair>;

fn authority() -> Authority {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}

/// Whether a client certificate is still within its dates.
#[derive(Debug)]
enum Validity {
    Current,
    Expired,
}

/// Which authority signs a client certificate.
enum Signer {
    Trusted,
    Unknown,
}

/// Writes a client certificate and returns a client that presents it.
fn client_with(
    files: &Files,
    issuer: &Authority,
    usage: &ExtendedKeyUsagePurpose,
    validity: &Validity,
) -> Client {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["client.test".to_owned()]).unwrap();
    params.extended_key_usages = vec![usage.clone()];
    if matches!(validity, Validity::Expired) {
        params.not_before = date_time_ymd(2000, 1, 1);
        params.not_after = date_time_ymd(2001, 1, 1);
    }
    let certificate = params.signed_by(&key, issuer).unwrap();
    let id = format!("{usage:?}-{validity:?}").replace(|c: char| !c.is_alphanumeric(), "");
    Client::builder()
        .danger_accept_invalid_certs()
        .identity_files(
            files.write(&format!("{id}.pem"), &certificate.pem()),
            files.write(&format!("{id}.key"), &key.serialize_pem()),
        )
        .build()
        .unwrap()
}

fn insecure_client() -> Client {
    Client::builder()
        .danger_accept_invalid_certs()
        .build()
        .unwrap()
}

fn start(tls: TlsConfig) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = Server::builder()
        .https_listener(listener)
        .tls(tls)
        .build()
        .unwrap();
    let url = format!("{}/api", server.urls()[0]);
    tokio::spawn(server.serve(std::future::pending()));
    url
}

async fn tls_of(client: &Client, url: &str) -> kneetap_api::TlsReport {
    let report: Report = client.report(url).await.unwrap();
    report.tls.unwrap()
}

#[tokio::test]
async fn generated_certificates_are_reported_as_such() {
    let url = start(TlsConfig::generated());

    let tls = tls_of(&insecure_client(), &url).await;

    assert_eq!(tls.certificate_source, CertificateSource::Generated);
    assert_eq!(tls.client_verification, ClientVerification::Absent);
    assert_eq!(tls.alpn.as_deref(), Some("h2"));
}

#[tokio::test]
async fn optional_client_certificates_are_never_called_verified() {
    let files = Files::new();
    let url = start(TlsConfig::generated());
    let client = client_with(
        &files,
        &authority(),
        &ExtendedKeyUsagePurpose::ClientAuth,
        &Validity::Current,
    );

    let tls = tls_of(&client, &url).await;

    assert_eq!(tls.client_verification, ClientVerification::Unverified);
    assert_eq!(tls.client_subjects, ["CN=rcgen self signed cert"]);
}

#[tokio::test]
async fn a_client_authority_accepts_a_certificate_it_vouches_for() {
    let files = Files::new();
    let trusted = authority();
    let url = start(TlsConfig::generated().client_ca(files.write("ca.pem", &trusted.pem())));
    let client = client_with(
        &files,
        &trusted,
        &ExtendedKeyUsagePurpose::ClientAuth,
        &Validity::Current,
    );

    let tls = tls_of(&client, &url).await;

    assert_eq!(tls.client_verification, ClientVerification::Verified);
}

#[tokio::test]
async fn a_client_authority_requires_a_certificate() {
    let files = Files::new();
    let url = start(TlsConfig::generated().client_ca(files.write("ca.pem", &authority().pem())));

    insecure_client().report(&url).await.unwrap_err();
}

#[rstest]
#[case::unknown_authority(
    Signer::Unknown,
    ExtendedKeyUsagePurpose::ClientAuth,
    Validity::Current
)]
#[case::expired(
    Signer::Trusted,
    ExtendedKeyUsagePurpose::ClientAuth,
    Validity::Expired
)]
#[case::wrong_usage(
    Signer::Trusted,
    ExtendedKeyUsagePurpose::ServerAuth,
    Validity::Current
)]
#[tokio::test]
async fn a_client_authority_refuses_certificates_it_does_not_vouch_for(
    #[case] signer: Signer,
    #[case] usage: ExtendedKeyUsagePurpose,
    #[case] validity: Validity,
) {
    let files = Files::new();
    let trusted = authority();
    let url = start(TlsConfig::generated().client_ca(files.write("ca.pem", &trusted.pem())));
    let other = authority();
    let issuer = match signer {
        Signer::Trusted => &trusted,
        Signer::Unknown => &other,
    };
    let client = client_with(&files, issuer, &usage, &validity);

    client.report(&url).await.unwrap_err();
}

#[tokio::test]
async fn supplied_certificates_verify_against_their_authority() {
    let files = Files::new();
    let issuer = authority();
    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    let certificate = params.signed_by(&key, &issuer).unwrap();
    let url = start(TlsConfig::files(
        files.write("server.pem", &certificate.pem()),
        files.write("server.key", &key.serialize_pem()),
    ));
    let client = Client::builder()
        .root_certificates_file(files.write("ca.pem", &issuer.pem()))
        .build()
        .unwrap();

    let tls = tls_of(&client, &url).await;

    assert_eq!(tls.certificate_source, CertificateSource::File);
}

/// Opens a gRPC channel over TLS to `address`, trusting only `issuer`.
async fn grpc_channel(address: SocketAddr, issuer: &Authority) -> Channel {
    let mut roots = RootCertStore::empty();
    roots.add(issuer.der().clone()).unwrap();
    let mut config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let connector = TlsConnector::from(Arc::new(config));
    Endpoint::from_static("http://127.0.0.1")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let connector = connector.clone();
            async move {
                let stream = TcpStream::connect(address).await?;
                let name = ServerName::from(address.ip());
                let stream = connector.connect(name, stream).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .unwrap()
}

#[tokio::test]
async fn shutdown_ends_open_health_watches_over_tls() {
    let files = Files::new();
    let issuer = authority();
    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    let certificate = params.signed_by(&key, &issuer).unwrap();
    let server = Server::builder()
        .https_listener(TcpListener::bind("127.0.0.1:0").unwrap())
        .tls(TlsConfig::files(
            files.write("server.pem", &certificate.pem()),
            files.write("server.key", &key.serialize_pem()),
        ))
        .build()
        .unwrap();
    let address = server.local_addr();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(async {
        stopped.await.ok();
    }));
    let request = HealthCheckRequest {
        service: String::new(),
    };
    let mut client = HealthClient::new(grpc_channel(address, &issuer).await);
    let mut watch = client.watch(request).await.unwrap().into_inner();
    watch.message().await.unwrap();

    stop.send(()).unwrap();

    let finished = tokio::time::timeout(Duration::from_secs(5), serving).await;
    finished.expect("still serving").unwrap().unwrap();
}

#[tokio::test]
async fn an_https_listener_requires_a_tls_configuration() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();

    let error = Server::builder()
        .https_listener(listener)
        .build()
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "an HTTPS listener needs a TLS configuration"
    );
}
