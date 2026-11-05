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
    /// A failed lookup is reported as missing data rather than an error, since a partial report is
    /// still useful.
    pub fn detect() -> Self {
        let name = hostname::get()
            .ok()
            .and_then(|name| name.into_string().ok());
        let addresses = if_addrs::get_if_addrs()
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
