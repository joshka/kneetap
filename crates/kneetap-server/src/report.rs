use std::collections::BTreeMap;
use std::net::SocketAddr;

use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use kneetap_api::{BodyEncoding, Report};

use crate::host::Host;

/// Builds a report from a request's head and its already-read body.
///
/// The host comes from the URI when it has an authority, as in HTTP/2, and otherwise from `Host`.
pub fn build(
    name: Option<String>,
    host: &Host,
    peer: SocketAddr,
    environment: Option<Vec<String>>,
    parts: &Parts,
    body: &[u8],
) -> Report {
    let mut headers = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in &parts.headers {
        headers
            .entry(name.to_string())
            .or_default()
            .push(header_text(value));
    }
    let (body, body_encoding) = match std::str::from_utf8(body) {
        Ok(text) => (text.to_owned(), BodyEncoding::Text),
        Err(_) => (STANDARD.encode(body), BodyEncoding::Base64),
    };
    Report {
        name,
        hostname: host.name.clone(),
        addresses: host.addresses.clone(),
        remote_addr: peer.to_string(),
        environment,
        method: parts.method.to_string(),
        target: parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string),
        host: parts
            .uri
            .authority()
            .map(ToString::to_string)
            .or_else(|| parts.headers.get(header::HOST).map(header_text))
            .unwrap_or_default(),
        http_version: format!("{:?}", parts.version),
        headers,
        body,
        body_encoding,
    }
}

/// Renders a header value, escaping bytes that aren't UTF-8 so a report can show them.
fn header_text(value: &HeaderValue) -> String {
    std::str::from_utf8(value.as_bytes()).map_or_else(
        |_| value.as_bytes().escape_ascii().to_string(),
        str::to_owned,
    )
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use rstest::rstest;

    use super::*;

    fn parts(request: Request<()>) -> Parts {
        request.into_parts().0
    }

    fn report(request: Request<()>, body: &[u8]) -> Report {
        let peer = "10.0.0.9:4000".parse().unwrap();
        build(None, &Host::default(), peer, None, &parts(request), body)
    }

    #[rstest]
    #[case(&[255, b'x'], "\\xffx")]
    #[case("café".as_bytes(), "café")]
    fn header_values_keep_utf8_and_escape_other_bytes(#[case] bytes: &[u8], #[case] text: &str) {
        let value = HeaderValue::from_bytes(bytes).unwrap();

        assert_eq!(header_text(&value), text);
    }

    #[test]
    fn repeated_headers_keep_every_value_in_order() {
        let request = Request::builder()
            .header("x-repeat", "one")
            .header("x-repeat", "two")
            .body(())
            .unwrap();

        assert_eq!(report(request, b"").headers["x-repeat"], ["one", "two"]);
    }

    #[rstest]
    #[case(b"hello", "hello", BodyEncoding::Text)]
    #[case(&[0xff, 0xfe], "//4=", BodyEncoding::Base64)]
    fn bodies_that_are_not_text_are_base64(
        #[case] bytes: &[u8],
        #[case] body: &str,
        #[case] encoding: BodyEncoding,
    ) {
        let report = report(Request::new(()), bytes);

        assert_eq!(
            (report.body.as_str(), report.body_encoding),
            (body, encoding)
        );
    }

    #[test]
    fn host_comes_from_the_header_when_the_uri_has_no_authority() {
        let request = Request::builder()
            .uri("/a?b=c")
            .header("host", "example.test")
            .body(())
            .unwrap();

        let report = report(request, b"");

        assert_eq!(
            (report.host.as_str(), report.target.as_str()),
            ("example.test", "/a?b=c")
        );
    }
}
