use kneetap_api::Report;

/// Exact header names that proxies, load balancers, CDNs, and meshes add on the way in.
const NAMES: &[&str] = &[
    "b3",
    "forwarded",
    "traceparent",
    "tracestate",
    "true-client-ip",
    "via",
    "x-amzn-trace-id",
    "x-real-ip",
    "x-request-id",
];

/// Header name prefixes that the same intermediaries use for families of headers.
const PREFIXES: &[&str] = &["cf-", "x-b3-", "x-envoy-", "x-forwarded-", "x-original-"];

/// Reports whether a header is one an intermediary usually adds, rather than the client.
pub fn is_added_by_proxy(name: &str) -> bool {
    NAMES.contains(&name) || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Returns the original client address a proxy claims, when it differs from the connected peer.
///
/// The first `X-Forwarded-For` entry is the client as the outermost proxy saw it. It's only a
/// claim, since any client can send the header.
pub fn forwarded_client(report: &Report) -> Option<&str> {
    let first = report.headers.get("x-forwarded-for")?.first()?;
    let client = first.split(',').next()?.trim();
    let peer_ip = peer_ip(&report.remote_addr);
    (!client.is_empty() && client != peer_ip).then_some(client)
}

/// Strips the port from a socket address, including the brackets around an IPv6 address.
fn peer_ip(remote_addr: &str) -> &str {
    let host = remote_addr
        .rsplit_once(':')
        .map_or(remote_addr, |(host, _)| host);
    host.trim_start_matches('[').trim_end_matches(']')
}

/// Reports whether a plain connection came from a proxy that says the client used HTTPS.
///
/// That means TLS ended at the proxy, which a reader would otherwise mistake for no TLS at all.
pub fn tls_ended_upstream(report: &Report) -> bool {
    if report.tls.is_some() {
        return false;
    }
    let values = |name| report.headers.get(name).into_iter().flatten();
    let forwarded_proto = values("x-forwarded-proto").any(|value| {
        value
            .split(',')
            .next()
            .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
    });
    // `Forwarded: proto=https` is the standard form (RFC 7239 section 5.4).
    let forwarded = values("forwarded").any(|value| {
        value
            .split([';', ','])
            .any(|pair| pair.trim().eq_ignore_ascii_case("proto=https"))
    });
    forwarded_proto || forwarded
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::html::tests::report;

    #[rstest]
    #[case("x-forwarded-for", true)]
    #[case("via", true)]
    #[case("x-envoy-attempt-count", true)]
    #[case("user-agent", false)]
    #[case("x-forwarded", false)]
    fn proxy_headers_are_recognised_by_name_or_prefix(#[case] name: &str, #[case] added: bool) {
        assert_eq!(is_added_by_proxy(name), added);
    }

    #[rstest]
    #[case("203.0.113.7, 10.0.0.2", "10.0.0.9:4000", Some("203.0.113.7"))]
    #[case("10.0.0.9", "10.0.0.9:4000", None)]
    #[case("::1", "[::1]:4000", None)]
    fn the_forwarded_client_is_shown_only_when_it_differs_from_the_peer(
        #[case] forwarded_for: &str,
        #[case] remote_addr: &str,
        #[case] client: Option<&str>,
    ) {
        let mut report = report();
        report.remote_addr = remote_addr.to_owned();
        report
            .headers
            .insert("x-forwarded-for".to_owned(), vec![forwarded_for.to_owned()]);

        assert_eq!(forwarded_client(&report), client);
    }

    #[rstest]
    #[case("x-forwarded-proto", "https", true)]
    #[case("x-forwarded-proto", "http", false)]
    #[case("forwarded", "for=203.0.113.7;proto=https", true)]
    #[case("forwarded", "for=203.0.113.7;proto=http", false)]
    fn https_at_the_proxy_means_tls_ended_upstream(
        #[case] name: &str,
        #[case] value: &str,
        #[case] upstream: bool,
    ) {
        let mut report = report();
        report
            .headers
            .insert(name.to_owned(), vec![value.to_owned()]);

        assert_eq!(tls_ended_upstream(&report), upstream);
    }
}
