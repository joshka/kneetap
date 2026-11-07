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

    /// Recorded in server status when this listener moved to its fallback.
    pub moved: &'static str,

    /// Recorded in server status when this listener didn't start.
    pub failed: &'static str,
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
    let may_fall_back = choice.explicit.is_none()
        && matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::AddrInUse
        );
    if !may_fall_back {
        degradations.push(choice.failed.to_owned());
        return Ok(None);
    }
    let fallback = bind_any(bind, choice.fallback)
        .inspect_err(|error| {
            tracing::warn!(%error, port = choice.fallback, "listener.fallback.failed");
        })
        .ok();
    let degradation = if fallback.is_some() {
        choice.moved
    } else {
        choice.failed
    };
    degradations.push(degradation.to_owned());
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

/// Reads the addresses of this host's interfaces, or none if the system can't list them.
pub fn interface_addresses() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .inspect_err(|error| tracing::warn!(%error, "server.addresses.unavailable"))
        .unwrap_or_default()
        .iter()
        .map(if_addrs::Interface::ip)
        .collect()
}

/// Lists the URLs a browser can open for the listeners at `urls`, since one on all addresses
/// reports `[::]` or `0.0.0.0`, which isn't a place to connect to.
///
/// URLs are grouped by address, nearest first, keeping the listeners' order within each address.
/// IPv6 link-local addresses are left out because a browser can't open them without a zone.
pub fn reachable_urls(urls: &[String], interfaces: &[IpAddr]) -> Vec<String> {
    let mut reachable: Vec<(IpAddr, String)> = urls
        .iter()
        .flat_map(|url| addresses_for(url, interfaces))
        .collect();
    reachable.sort_by_key(|(address, _)| (nearness(*address), *address));
    reachable.dedup();
    reachable.into_iter().map(|(_, url)| url).collect()
}

/// Lists each address a listener accepts connections on, with its URL.
fn addresses_for(url: &str, interfaces: &[IpAddr]) -> Vec<(IpAddr, String)> {
    let Some((scheme, address)) = url.split_once("://") else {
        return Vec::new();
    };
    let Ok(listening) = address.parse::<SocketAddr>() else {
        return Vec::new();
    };
    let accepted: Vec<IpAddr> = interfaces
        .iter()
        .copied()
        .filter(|address| accepts(listening.ip(), *address))
        .collect();
    if !listening.ip().is_unspecified() || accepted.is_empty() {
        return vec![(listening.ip(), url.to_owned())];
    }
    accepted
        .into_iter()
        .map(|address| {
            let url = format!("{scheme}://{}", SocketAddr::new(address, listening.port()));
            (address, url)
        })
        .collect()
}

/// Ranks an address by how close a client using it is likely to be: this host, then the local
/// network, then anything else over IPv4, then IPv6.
fn nearness(address: IpAddr) -> u8 {
    match address {
        _ if address.is_loopback() => 0,
        IpAddr::V4(address) if address.is_private() => 1,
        IpAddr::V4(_) => 2,
        IpAddr::V6(_) => 3,
    }
}

/// Whether a listener on `listening` accepts connections to `address`. An IPv6 listener on all
/// addresses accepts IPv4 too, because `listen` turns off `IPV6_V6ONLY`.
fn accepts(listening: IpAddr, address: IpAddr) -> bool {
    match address {
        IpAddr::V4(_) => true,
        IpAddr::V6(address) => listening.is_ipv6() && !address.is_unicast_link_local(),
    }
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
            moved: "http_port_moved",
            failed: "http_bind_failed",
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
        assert_eq!(degradations, ["http_port_moved"]);
    }

    #[test]
    fn a_listener_without_a_usable_fallback_is_reported_as_off() {
        let (_held, taken, _free) = taken_and_free();
        let mut degradations = Vec::new();

        let choice = choice(None, taken, taken);
        let listener = open(LOOPBACK, choice, Strictness::Lenient, &mut degradations).unwrap();

        assert_eq!(
            listener.map(|listener| listener.local_addr().unwrap()),
            None
        );
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

    fn interfaces() -> Vec<IpAddr> {
        [
            "fd7a::5",
            "100.120.0.5",
            "192.168.1.5",
            "::1",
            "fe80::1",
            "127.0.0.1",
            "192.168.1.5",
        ]
        .map(|ip| ip.parse().unwrap())
        .to_vec()
    }

    fn urls(urls: &[&str]) -> Vec<String> {
        urls.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn listeners_on_all_addresses_are_grouped_by_address_nearest_first() {
        let listeners = urls(&["http://[::]:80", "https://[::]:443"]);

        let urls = reachable_urls(&listeners, &interfaces());

        assert_eq!(
            urls,
            [
                "http://127.0.0.1:80",
                "https://127.0.0.1:443",
                "http://[::1]:80",
                "https://[::1]:443",
                "http://192.168.1.5:80",
                "https://192.168.1.5:443",
                "http://100.120.0.5:80",
                "https://100.120.0.5:443",
                "http://[fd7a::5]:80",
                "https://[fd7a::5]:443",
            ]
        );
    }

    #[test]
    fn an_ipv4_listener_lists_only_ipv4_addresses() {
        let listeners = urls(&["https://0.0.0.0:8443"]);

        let urls = reachable_urls(&listeners, &interfaces());

        assert_eq!(
            urls,
            [
                "https://127.0.0.1:8443",
                "https://192.168.1.5:8443",
                "https://100.120.0.5:8443"
            ]
        );
    }

    #[test]
    fn a_specific_address_is_listed_as_is() {
        let listeners = urls(&["http://127.0.0.1:8080"]);

        let urls = reachable_urls(&listeners, &interfaces());

        assert_eq!(urls, ["http://127.0.0.1:8080"]);
    }

    #[test]
    fn a_listener_is_still_listed_when_interfaces_are_unknown() {
        let listeners = urls(&["http://[::]:8080"]);

        let urls = reachable_urls(&listeners, &[]);

        assert_eq!(urls, ["http://[::]:8080"]);
    }
}
