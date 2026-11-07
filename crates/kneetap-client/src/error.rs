use std::error;
use std::fmt::{self, Display, Formatter};

/// A failure to query a server.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    source: Option<Box<dyn error::Error + Send + Sync>>,
}

/// Which step failed, with the details `Display` needs.
#[derive(Debug)]
enum ErrorKind {
    Build,
    Tls,
    Request(String),
    Decode(String),
    TooLarge(String),
    Unhealthy(String, u16),
    Unrelated(String),
    NoCandidates,
}

impl Error {
    pub(crate) fn build(source: reqwest::Error) -> Self {
        Self::new(ErrorKind::Build, Some(source.into()))
    }

    pub(crate) fn tls(source: impl Into<Box<dyn error::Error + Send + Sync>>) -> Self {
        Self::new(ErrorKind::Tls, Some(source.into()))
    }

    pub(crate) fn request(url: &str, source: reqwest::Error) -> Self {
        Self::new(ErrorKind::Request(url.to_owned()), Some(source.into()))
    }

    pub(crate) fn decode(url: &str, source: serde_json::Error) -> Self {
        Self::new(ErrorKind::Decode(url.to_owned()), Some(source.into()))
    }

    pub(crate) fn too_large(url: &str) -> Self {
        Self::new(ErrorKind::TooLarge(url.to_owned()), None)
    }

    pub(crate) fn unhealthy(url: &str, status: u16) -> Self {
        Self::new(ErrorKind::Unhealthy(url.to_owned(), status), None)
    }

    pub(crate) fn unrelated(url: &str) -> Self {
        Self::new(ErrorKind::Unrelated(url.to_owned()), None)
    }

    pub(crate) const fn no_candidates() -> Self {
        Self::new(ErrorKind::NoCandidates, None)
    }

    const fn new(kind: ErrorKind, source: Option<Box<dyn error::Error + Send + Sync>>) -> Self {
        Self { kind, source }
    }

    /// Returns the status a server reported if it answered as unhealthy.
    #[must_use]
    pub const fn unhealthy_status(&self) -> Option<u16> {
        match self.kind {
            ErrorKind::Unhealthy(_, status) => Some(status),
            _ => None,
        }
    }

    /// Returns whether something other than the intended kneetap server answered.
    #[must_use]
    pub const fn is_unrelated(&self) -> bool {
        matches!(self.kind, ErrorKind::Unrelated(_))
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ErrorKind::Build => write!(f, "failed to build the client"),
            ErrorKind::Tls => write!(f, "failed to set up TLS"),
            ErrorKind::Request(url) => write!(f, "request to {url} failed"),
            ErrorKind::Decode(url) => write!(f, "{url} didn't return a report"),
            ErrorKind::TooLarge(url) => write!(f, "{url} returned too much data for a report"),
            ErrorKind::Unhealthy(url, status) => write!(f, "{url} returned status {status}"),
            ErrorKind::Unrelated(url) => write!(f, "{url} isn't the expected kneetap"),
            ErrorKind::NoCandidates => write!(f, "no health endpoint to check"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn error::Error + 'static))
    }
}
