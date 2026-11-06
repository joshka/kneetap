use std::ffi::OsString;
use std::net::IpAddr;

/// Identity of the machine serving requests.
///
/// It's looked up once at startup, because a request shouldn't pay for system calls whose answers
/// rarely change, and because tests can then supply fixed values.
#[derive(Clone, Debug, Default)]
pub struct Host {
    pub name: Option<String>,
    pub addresses: Vec<String>,
}

impl Host {
    /// Reads the host's identity, leaving out whatever the system can't provide.
    ///
    /// A failed lookup is logged and reported as missing data rather than an error, since a partial
    /// report is still useful.
    pub fn detect() -> Self {
        let name = hostname::get()
            .inspect_err(|error| tracing::warn!(%error, "host.name.unavailable"))
            .ok()
            .and_then(unicode_name);
        let addresses = if_addrs::get_if_addrs()
            .inspect_err(|error| tracing::warn!(%error, "host.addresses.unavailable"))
            .unwrap_or_default()
            .iter()
            .map(if_addrs::Interface::ip)
            .collect();
        Self {
            name,
            addresses: sorted(addresses),
        }
    }
}

/// Converts a hostname for reports, which can only carry Unicode, logging one that isn't.
fn unicode_name(name: OsString) -> Option<String> {
    name.into_string()
        .inspect_err(|name| tracing::warn!(?name, "host.name.not_unicode"))
        .ok()
}

/// Puts addresses in a stable order, because interfaces can share one and the order varies.
fn sorted(mut addresses: Vec<IpAddr>) -> Vec<String> {
    addresses.sort_unstable();
    addresses.dedup();
    addresses.iter().map(ToString::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_sorted_without_duplicates() {
        let addresses = ["10.0.0.2", "10.0.0.1", "10.0.0.2"].map(|ip| ip.parse().unwrap());

        assert_eq!(sorted(addresses.to_vec()), ["10.0.0.1", "10.0.0.2"]);
    }
}
