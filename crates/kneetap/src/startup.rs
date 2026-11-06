use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};

/// Deep enough to absorb the bursts a load test sends.
const LISTEN_BACKLOG: i32 = 1024;

pub const DEFAULT_HTTP_PORT: u16 = 80;
pub const FALLBACK_HTTP_PORT: u16 = 8080;
pub const DEFAULT_TLS_PORT: u16 = 443;
pub const FALLBACK_TLS_PORT: u16 = 8443;

/// A port to listen on, and what to try if it's unavailable.
#[derive(Clone, Copy, Debug)]
pub struct PortChoice {
    /// The port the operator asked for, which is never silently replaced.
    pub explicit: Option<u16>,
    pub default: u16,
    pub fallback: u16,

    /// Recorded in server status when this listener can't start normally.
    pub degradation: &'static str,
}

/// Whether a listener that can't start normally stops the server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Strictness {
    /// Carry on with what works, and report what was worked around.
    Lenient,

    /// Exit rather than start in any way the operator didn't ask for.
    Strict,
}

/// Binds one listener, keeping the server useful when its port isn't available.
///
/// An unprivileged process can't bind port 80, so an implicit port moves to its fallback. A
/// listener that still can't start is left out, unless [`Strictness::Strict`] asks for a failure
/// instead.
pub fn open(
    bind: Option<IpAddr>,
    choice: PortChoice,
    strictness: Strictness,
    degradations: &mut Vec<String>,
) -> Result<Option<TcpListener>> {
    let port = choice.explicit.unwrap_or(choice.default);
    let error = match bind_any(bind, port) {
        Ok(listener) => return Ok(Some(listener)),
        Err(error) => error,
    };
    // A missing address is a configuration mistake that no other port can fix.
    if strictness == Strictness::Strict || error.kind() == io::ErrorKind::AddrNotAvailable {
        return Err(error).with_context(|| format!("couldn't listen on port {port}"));
    }
    tracing::warn!(%error, port, "listener.bind.failed");
    degradations.push(choice.degradation.to_owned());
    let may_fall_back = choice.explicit.is_none()
        && matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::AddrInUse
        );
    if !may_fall_back {
        return Ok(None);
    }
    let fallback = bind_any(bind, choice.fallback)
        .inspect_err(|error| {
            tracing::warn!(%error, port = choice.fallback, "listener.fallback.failed");
        })
        .ok();
    Ok(fallback)
}

/// Binds the requested address, or both IP families when none was requested.
fn bind_any(address: Option<IpAddr>, port: u16) -> io::Result<TcpListener> {
    if let Some(address) = address {
        return listen(SocketAddr::new(address, port));
    }
    listen(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port)).or_else(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::AddrInUse
        ) {
            return Err(error);
        }
        // Hosts without IPv6 still deserve a working server.
        listen(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))
    })
}

/// Opens a socket directly, since the standard library can't accept both IP families on one.
fn listen(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    if address.is_ipv6() && address.ip().is_unspecified() {
        socket.set_only_v6(false)?;
    }
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&address.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOPBACK: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

    /// Holds a port so that binding it fails, and returns a second free port.
    fn taken_and_free() -> (TcpListener, u16, u16) {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        (taken, port, free.local_addr().unwrap().port())
    }

    fn choice(explicit: Option<u16>, default: u16, fallback: u16) -> PortChoice {
        PortChoice {
            explicit,
            default,
            fallback,
            degradation: "http_bind_failed",
        }
    }

    #[test]
    fn an_implicit_port_moves_to_its_fallback() {
        let (_held, taken, free) = taken_and_free();
        let mut degradations = Vec::new();

        let listener = open(
            LOOPBACK,
            choice(None, taken, free),
            Strictness::Lenient,
            &mut degradations,
        );

        let port = listener.unwrap().unwrap().local_addr().unwrap().port();
        assert_eq!(port, free);
        assert_eq!(degradations, ["http_bind_failed"]);
    }

    #[test]
    fn an_explicit_port_is_never_replaced() {
        let (_held, taken, free) = taken_and_free();
        let mut degradations = Vec::new();

        let choice = choice(Some(taken), taken, free);
        let listener = open(LOOPBACK, choice, Strictness::Lenient, &mut degradations).unwrap();

        assert_eq!(
            listener.map(|listener| listener.local_addr().unwrap()),
            None
        );
        assert_eq!(degradations, ["http_bind_failed"]);
    }

    #[test]
    fn strict_fails_instead_of_moving() {
        let (_held, taken, free) = taken_and_free();

        let choice = choice(None, taken, free);
        let result = open(LOOPBACK, choice, Strictness::Strict, &mut Vec::new());

        let error = result.unwrap_err().to_string();
        assert_eq!(error, format!("couldn't listen on port {taken}"));
    }

    #[test]
    fn an_unavailable_address_is_a_mistake_not_a_degradation() {
        let address = Some("192.0.2.1".parse().unwrap());

        let choice = choice(None, 0, 0);
        let result = open(address, choice, Strictness::Lenient, &mut Vec::new());

        let error = result.unwrap_err().to_string();
        assert_eq!(error, "couldn't listen on port 0");
    }
}
