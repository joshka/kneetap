use std::time::Duration;

use kneetap_api::{HEALTH_HEADER, HEALTH_MARKER, Report};
use reqwest::redirect::Policy;

use crate::Error;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest report read from a server, which needn't be trusted to stop sending.
///
/// This leaves room for a base64 copy of a default 1 MiB body, plus headers and the environment.
const MAX_REPORT_SIZE: usize = 8 << 20;

/// A handle for querying kneetap servers, which is cheap to clone.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
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

    /// Checks the first candidate that is a kneetap health endpoint.
    ///
    /// Candidates that can't be reached, or that belong to another service, are skipped. An
    /// unhealthy kneetap ends the search, so that another candidate can't mask it.
    ///
    /// # Errors
    ///
    /// Returns an error if kneetap answered as unhealthy, or no candidate was a kneetap server, in
    /// which case the error describes the last candidate.
    pub async fn discover_health(&self, candidates: &[String]) -> Result<(), Error> {
        let mut last_error = Error::no_candidates();
        for url in candidates {
            match self.send(url).await {
                Ok(response) if is_kneetap(&response) => return verdict(url, &response),
                Ok(_) => last_error = Error::unrelated(url),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    async fn send(&self, url: &str) -> Result<reqwest::Response, Error> {
        let response = self.http.get(url).send().await;
        response.map_err(|source| Error::request(url, source))
    }
}

/// Tells kneetap from another service that happens to answer on the same port.
fn is_kneetap(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(HEALTH_HEADER)
        .is_some_and(|value| value == HEALTH_MARKER)
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
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
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

    /// Builds the client.
    ///
    /// Redirects aren't followed and proxies aren't used, because a probe should report what the
    /// target itself answered.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client can't be initialized.
    pub fn build(self) -> Result<Client, Error> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(self.timeout)
            .build()
            .map_err(Error::build)?;
        Ok(Client { http })
    }
}
