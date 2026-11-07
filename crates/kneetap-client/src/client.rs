use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use kneetap_api::{HEALTH_HEADER, HEALTH_MARKER, HEALTH_NAME_HEADER, Report};
use reqwest::redirect::Policy;
use rustls::pki_types::ServerName;
use rustls::{AlertDescription, ClientConfig};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};
use tokio_rustls::TlsConnector;

use crate::Error;
use crate::tls::{self, Trust};

/// Long enough for a slow server, short enough for a health probe to give a useful answer.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest report read from a server, which needn't be trusted to stop sending.
///
/// This leaves room for a base64 copy of a default 1 MiB body, plus headers and the environment.
const MAX_REPORT_SIZE: usize = 8 << 20;

/// How long discovery waits for other candidates after a healthy answer.
///
/// Local servers answer well within this, so an unhealthy one still gets to fail the check, while a
/// silent service on another port can't hold it up.
const SETTLE_TIME: Duration = Duration::from_millis(250);

/// A handle for querying kneetap servers, which is cheap to clone.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    tls: Arc<ClientConfig>,
    timeout: Duration,
}

impl Client {
    /// Starts configuring a client.
    #[must_use]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Fetches a report from the JSON endpoint at `url`, normally a server's `/api`.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails, or the response is too large or isn't a report.
    pub async fn report(&self, url: &str) -> Result<Report, Error> {
        let request_error = |source| Error::request(url, source);
        let response = self.send(url).await?;
        let mut response = response.error_for_status().map_err(request_error)?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(request_error)? {
            if body.len() + chunk.len() > MAX_REPORT_SIZE {
                return Err(Error::too_large(url));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|source| Error::decode(url, source))
    }

    /// Checks the health endpoint at `url`, which may belong to any service.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or the status isn't 2xx.
    pub async fn check_health(&self, url: &str) -> Result<(), Error> {
        verdict(url, &self.send(url).await?)
    }

    /// Checks every candidate at once, for the kneetap server called `name` if one is given.
    ///
    /// Candidates that can't be reached, or that belong to another service or another name, are
    /// skipped. An unhealthy match ends the search, so that another candidate can't mask it. After
    /// a healthy match, the rest get a moment to answer before the check passes without them.
    ///
    /// # Errors
    ///
    /// Returns an error if a match answered as unhealthy, or nothing matched. The error then
    /// describes the last candidate.
    pub async fn discover_health(
        &self,
        candidates: &[String],
        name: Option<&str>,
    ) -> Result<(), Error> {
        let mut probes: FuturesUnordered<_> = candidates
            .iter()
            .enumerate()
            .map(|(index, url)| async move { (index, url, self.send(url).await) })
            .collect();
        let mut failure = Failure::default();
        let mut settled_at = None;
        while let Some((index, url, result)) = next_probe(&mut probes, settled_at).await {
            match result {
                Ok(response) if is_match(&response, name) => {
                    verdict(url, &response)?;
                    settled_at.get_or_insert_with(|| Instant::now() + SETTLE_TIME);
                }
                Ok(_) => failure.record(index, Error::unrelated(url)),
                Err(error) => failure.record(index, error),
            }
        }
        match settled_at {
            Some(_) => Ok(()),
            None => Err(failure.error),
        }
    }

    /// Returns whether a TLS server at any of `addresses` refuses clients without a certificate.
    ///
    /// This shows that some server is listening, but not which one or whether it's healthy. They're
    /// tried at once, so a server that waits for a request can't hold up the others, and each gets
    /// the client's timeout.
    pub async fn requires_client_certificate(&self, addresses: &[SocketAddr]) -> bool {
        let mut probes: FuturesUnordered<_> = addresses
            .iter()
            .map(|address| self.refuses_without_certificate(*address))
            .collect();
        while let Some(refused) = probes.next().await {
            if refused {
                return true;
            }
        }
        false
    }

    /// Connects directly, because HTTP clients can't see why a closing connection closed.
    async fn refuses_without_certificate(&self, address: SocketAddr) -> bool {
        let connector = TlsConnector::from(self.tls.clone());
        let name = ServerName::IpAddress(address.ip().into());
        let outcome = async {
            let stream = TcpStream::connect(address).await?;
            let mut stream = connector.connect(name, stream).await?;
            // A TLS 1.3 client finishes its handshake first, so the refusal comes on the first
            // read.
            stream.read(&mut [0; 1]).await
        };
        let outcome = tokio::time::timeout(self.timeout, outcome).await;
        outcome.is_ok_and(|read| read.is_err_and(|error| is_certificate_demand(&error)))
    }

    /// Sends a `GET`. Any response counts as success here, because callers judge its status.
    async fn send(&self, url: &str) -> Result<reqwest::Response, Error> {
        let response = self.http.get(url).send().await;
        response.map_err(|source| Error::request(url, source))
    }
}

/// Waits for the next answer, or returns `None` once none remain or `settled_at` passes.
async fn next_probe<S>(probes: &mut S, settled_at: Option<Instant>) -> Option<S::Item>
where
    S: futures_util::Stream + Unpin,
{
    match settled_at {
        Some(deadline) => timeout_at(deadline, probes.next()).await.ok().flatten(),
        None => probes.next().await,
    }
}

/// The error discovery reports, which doesn't depend on the order that answers arrive in.
struct Failure {
    index: usize,
    error: Error,
}

impl Default for Failure {
    fn default() -> Self {
        Self {
            index: 0,
            error: Error::no_candidates(),
        }
    }
}

impl Failure {
    /// Keeps the error of the latest candidate in the list.
    fn record(&mut self, index: usize, error: Error) {
        if index >= self.index {
            *self = Self { index, error };
        }
    }
}

/// Recognizes `certificate_required`, the alert for a client with no certificate (RFC 8446 §6.2).
fn is_certificate_demand(error: &io::Error) -> bool {
    let cause = error.get_ref().and_then(|cause| cause.downcast_ref());
    matches!(
        cause,
        Some(rustls::Error::AlertReceived(
            AlertDescription::CertificateRequired
        ))
    )
}

/// Tells the intended kneetap from another service, or another kneetap, on a candidate port.
fn is_match(response: &reqwest::Response, name: Option<&str>) -> bool {
    let headers = response.headers();
    let is_kneetap = headers
        .get(HEALTH_HEADER)
        .is_some_and(|value| value == HEALTH_MARKER);
    let has_name = |name: &str| {
        headers
            .get(HEALTH_NAME_HEADER)
            .is_some_and(|value| value.as_bytes() == name.as_bytes())
    };
    is_kneetap && name.is_none_or(has_name)
}

/// Treats any 2xx as healthy, the same rule load balancers apply.
fn verdict(url: &str, response: &reqwest::Response) -> Result<(), Error> {
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(Error::unhealthy(url, status.as_u16()))
    }
}

/// Configures a [`Client`].
#[derive(Debug)]
pub struct ClientBuilder {
    timeout: Duration,
    trust: Trust,
    identity: Option<(PathBuf, PathBuf)>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            trust: Trust::default(),
            identity: None,
        }
    }
}

impl ClientBuilder {
    /// Sets how long one request may take. The default is 5 seconds.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Accepts any server certificate.
    ///
    /// Use this for a server's generated certificate on a trusted path, such as loopback. The
    /// connection is encrypted, but the server's identity isn't checked. This replaces
    /// any earlier [`root_certificates_file`](Self::root_certificates_file).
    #[must_use]
    pub fn danger_accept_invalid_certs(mut self) -> Self {
        self.trust = Trust::Any;
        self
    }

    /// Trusts only the authorities in this PEM bundle, instead of the built-in public roots.
    #[must_use]
    pub fn root_certificates_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.trust = Trust::Bundle(path.into());
        self
    }

    /// Presents a client certificate from PEM files.
    #[must_use]
    pub fn identity_files(
        mut self,
        certificate: impl Into<PathBuf>,
        key: impl Into<PathBuf>,
    ) -> Self {
        self.identity = Some((certificate.into(), key.into()));
        self
    }

    /// Builds the client.
    ///
    /// Redirects aren't followed and proxies aren't used, because a probe should report what the
    /// target itself answered.
    ///
    /// # Errors
    ///
    /// Returns an error if a certificate can't be read, or the HTTP client can't be initialized.
    pub fn build(self) -> Result<Client, Error> {
        let tls = tls::config(&self.trust, self.identity.as_ref())?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(self.timeout)
            .tls_backend_preconfigured(tls.clone())
            .build()
            .map_err(Error::build)?;
        let tls = Arc::new(tls);
        let timeout = self.timeout;
        Ok(Client { http, tls, timeout })
    }
}
