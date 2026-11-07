use kneetap_api::{BodyEncoding, CertificateSource, Report};

/// Headers the command leaves out: curl sets them itself, they'd change what comes back, or, for
/// `cookie`, a browser on a shared host such as localhost fills them with other sites' cookies.
const SKIPPED: &[&str] = &[
    "accept",
    "connection",
    "content-length",
    "cookie",
    "host",
    "keep-alive",
    "te",
    "transfer-encoding",
    "upgrade",
];

/// Writes a curl command that sends this request again, with the headers and body it arrived with.
///
/// `Accept` is left out so the command gets the text report rather than this page, and `Cookie`
/// because it's often long and not this site's. A body that wasn't UTF-8 is left out too, because
/// the report only holds it as base64.
pub fn command(report: &Report) -> String {
    // A proxy that ended TLS was reached over HTTPS. Trusting its header is safe here: a false
    // claim makes the command fail, never send in plain text what the client sent encrypted.
    let https = report.tls.is_some() || super::proxy::tls_ended_upstream(report);
    let scheme = if https { "https" } else { "http" };
    let mut parts = vec!["curl".to_owned()];
    // Only a generated certificate needs this. An operator's certificate should still be checked,
    // so a repeated request carrying credentials can't go to an impostor.
    let generated = report.tls.as_ref().map(|tls| tls.certificate_source);
    if generated == Some(CertificateSource::Generated) {
        parts.push("--insecure".to_owned());
    }
    // curl would otherwise expand braces and brackets into several requests.
    if report.target.contains(['{', '}', '[', ']']) {
        parts.push("--globoff".to_owned());
    }
    let has_text_body = !report.body.is_empty() && report.body_encoding == BodyEncoding::Text;
    let implied = if has_text_body { "POST" } else { "GET" };
    if report.method != implied {
        parts.push(format!("-X {}", quote(&report.method)));
    }
    for (name, values) in &report.headers {
        if SKIPPED.contains(&name.as_str()) {
            continue;
        }
        for value in values {
            parts.push(format!("-H {}", quote(&format!("{name}: {value}"))));
        }
    }
    // `--data-raw` sends a body that starts with `@` as written, rather than reading a file.
    if has_text_body {
        parts.push(format!("--data-raw {}", quote(&report.body)));
    }
    parts.push(quote(&format!(
        "{scheme}://{}{}",
        report.host, report.target
    )));
    parts.join(" \\\n  ")
}

/// Quotes a value for a POSIX shell, so it's passed as one argument with nothing expanded.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use kneetap_api::{ClientVerification, TlsReport};

    use super::*;
    use crate::html::tests::report;

    #[test]
    fn a_get_keeps_its_headers_but_not_accept_or_cookies() {
        let mut report = report();
        report
            .headers
            .insert("accept".to_owned(), vec!["text/html".to_owned()]);
        report
            .headers
            .insert("cookie".to_owned(), vec!["session=1".to_owned()]);
        report
            .headers
            .insert("x-trace".to_owned(), vec!["it's".to_owned()]);

        insta::assert_snapshot!(command(&report), @r"
        curl \
          -H 'x-trace: it'\''s' \
          'http://example.test/a?b=c'
        ");
    }

    #[test]
    fn a_request_whose_tls_ended_at_a_proxy_repeats_over_https() {
        let mut report = report();
        report
            .headers
            .insert("x-forwarded-proto".to_owned(), vec!["https".to_owned()]);

        insta::assert_snapshot!(command(&report), @r"
        curl \
          -H 'x-forwarded-proto: https' \
          'https://example.test/a?b=c'
        ");
    }

    #[test]
    fn an_operator_certificate_is_verified_and_globs_are_sent_literally() {
        let report = Report {
            target: "/a?item={one,two}".to_owned(),
            tls: Some(TlsReport {
                version: "TLSv1_3".to_owned(),
                cipher: "TLS13_AES_128_GCM_SHA256".to_owned(),
                alpn: None,
                certificate_source: CertificateSource::File,
                client_verification: ClientVerification::Absent,
                client_subjects: Vec::new(),
            }),
            ..report()
        };

        insta::assert_snapshot!(command(&report), @r"
        curl \
          --globoff \
          'https://example.test/a?item={one,two}'
        ");
    }

    #[test]
    fn a_tls_request_with_a_body_keeps_its_method_and_body() {
        let report = Report {
            method: "PUT".to_owned(),
            body: "{\"a\":1}".to_owned(),
            tls: Some(TlsReport {
                version: "TLSv1_3".to_owned(),
                cipher: "TLS13_AES_128_GCM_SHA256".to_owned(),
                alpn: None,
                certificate_source: CertificateSource::Generated,
                client_verification: ClientVerification::Absent,
                client_subjects: Vec::new(),
            }),
            ..report()
        };

        insta::assert_snapshot!(command(&report), @r#"
        curl \
          --insecure \
          -X 'PUT' \
          --data-raw '{"a":1}' \
          'https://example.test/a?b=c'
        "#);
    }
}
