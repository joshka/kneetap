use std::fmt::{self, Display, Formatter};
use std::{error, io};

/// A failure to start or run a server.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    source: Option<Box<dyn error::Error + Send + Sync>>,
}

/// What the server was doing when it failed.
#[derive(Debug)]
enum ErrorKind {
    Bind,
    Serve,
    NoListeners,
    MissingTls,
    TlsIdentity,
    ClientCa,
}

impl Error {
    pub(crate) fn bind(source: io::Error) -> Self {
        Self::new(ErrorKind::Bind, Some(source.into()))
    }

    pub(crate) fn serve(source: io::Error) -> Self {
        Self::new(ErrorKind::Serve, Some(source.into()))
    }

    pub(crate) const fn no_listeners() -> Self {
        Self::new(ErrorKind::NoListeners, None)
    }

    pub(crate) const fn missing_tls() -> Self {
        Self::new(ErrorKind::MissingTls, None)
    }

    pub(crate) fn tls_identity(source: impl Into<Box<dyn error::Error + Send + Sync>>) -> Self {
        Self::new(ErrorKind::TlsIdentity, Some(source.into()))
    }

    pub(crate) fn client_ca(source: impl Into<Box<dyn error::Error + Send + Sync>>) -> Self {
        Self::new(ErrorKind::ClientCa, Some(source.into()))
    }

    const fn new(kind: ErrorKind, source: Option<Box<dyn error::Error + Send + Sync>>) -> Self {
        Self { kind, source }
    }

    /// Returns whether the listening socket couldn't be set up.
    #[must_use]
    pub const fn is_bind(&self) -> bool {
        matches!(self.kind, ErrorKind::Bind)
    }

    /// Returns whether the server's certificate or key was unusable.
    ///
    /// A caller may fall back to a generated certificate after this error. It's never returned for
    /// a client authority problem, which must not weaken into accepting any client.
    #[must_use]
    pub const fn is_tls_identity(&self) -> bool {
        matches!(self.kind, ErrorKind::TlsIdentity)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self.kind {
            ErrorKind::Bind => "failed to bind listener",
            ErrorKind::Serve => "failed to serve connections",
            ErrorKind::NoListeners => "no listeners were configured",
            ErrorKind::MissingTls => "an HTTPS listener needs a TLS configuration",
            ErrorKind::TlsIdentity => "failed to load the TLS certificate or key",
            ErrorKind::ClientCa => "failed to load the client certificate authorities",
        })
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}
