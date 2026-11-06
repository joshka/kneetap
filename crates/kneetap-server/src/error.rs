use std::fmt::{self, Display, Formatter};
use std::{error, io};

/// A failure to start or run a server.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    source: io::Error,
}

/// What the server was doing when it failed.
#[derive(Debug)]
enum ErrorKind {
    Bind,
    Serve,
}

impl Error {
    pub(crate) const fn bind(source: io::Error) -> Self {
        Self {
            kind: ErrorKind::Bind,
            source,
        }
    }

    pub(crate) const fn serve(source: io::Error) -> Self {
        Self {
            kind: ErrorKind::Serve,
            source,
        }
    }

    /// Returns whether the listening socket couldn't be set up.
    #[must_use]
    pub const fn is_bind(&self) -> bool {
        matches!(self.kind, ErrorKind::Bind)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self.kind {
            ErrorKind::Bind => "failed to bind listener",
            ErrorKind::Serve => "failed to serve connections",
        })
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.source)
    }
}
