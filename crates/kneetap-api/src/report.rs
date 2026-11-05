use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

/// The answer to one request: which instance handled it, and what arrived.
///
/// Comparing a report with what the client sent shows what a proxy or load balancer changed on the
/// way, and comparing reports across requests shows how traffic is being spread.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// The operator's name for this instance, which distinguishes replicas on one host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The host's name, or `None` if it couldn't be determined.
    pub hostname: Option<String>,

    /// The host's interface addresses, sorted.
    pub addresses: Vec<String>,

    /// The address of the connected peer, which is the proxy when there is one.
    pub remote_addr: String,

    /// The server's environment as `NAME=value` lines, when the operator allowed it and the client
    /// asked for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<Vec<String>>,

    /// The request method.
    pub method: String,

    /// The path and query as received.
    pub target: String,

    /// The authority the client asked for, from the URI or the `Host` header.
    pub host: String,

    /// The HTTP version, such as `HTTP/1.1`.
    pub http_version: String,

    /// Header values by lowercase name, in the order received.
    pub headers: BTreeMap<String, Vec<String>>,

    /// The request body, encoded as [`body_encoding`](Self::body_encoding) says.
    pub body: String,

    /// How [`body`](Self::body) is encoded.
    pub body_encoding: BodyEncoding,

    /// The TLS session, or `None` for a plain connection.
    pub tls: Option<TlsReport>,

    /// Problems the server is working around.
    #[serde(default)]
    pub server_status: ServerStatus,
}

/// Problems a server is working around while it keeps serving.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    /// Why the last certificate reload was rejected. The previous certificate stays in use.
    pub reload_error: Option<String>,

    /// Codes for what didn't start normally, such as `http_bind_failed`.
    #[serde(default)]
    pub degradations: Vec<String>,
}

/// What was negotiated on a TLS connection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsReport {
    /// The protocol version, such as `TLSv1_3`.
    pub version: String,

    /// The cipher suite.
    pub cipher: String,

    /// The application protocol chosen during the handshake.
    pub alpn: Option<String>,

    /// Where the server's certificate came from.
    pub certificate_source: CertificateSource,

    /// How far the client's certificate was trusted.
    pub client_verification: ClientVerification,

    /// The subject of each certificate the client presented.
    pub client_subjects: Vec<String>,
}

/// Where a server's certificate came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CertificateSource {
    /// The operator supplied it.
    File,

    /// The server generated a self-signed one.
    Generated,
}

/// How far a client certificate was trusted.
///
/// `Unverified` subjects are only claims, so never use them for authorization.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientVerification {
    /// The client presented no certificate.
    Absent,

    /// The certificate chains to a configured authority.
    Verified,

    /// The client proved it holds the key, but nothing vouches for the certificate.
    Unverified,
}

impl Display for ClientVerification {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Absent => "absent",
            Self::Verified => "verified",
            Self::Unverified => "unverified",
        })
    }
}

/// How a report carries a request body, which may not be valid text.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyEncoding {
    /// The body was valid UTF-8 and is included unchanged.
    Text,

    /// The body wasn't valid UTF-8, so it's included as standard base64.
    Base64,
}

/// Writes the report as text, up to the blank line that precedes a body.
///
/// The body is left out because text responses end with its original bytes, which a [`Report`] may
/// only hold as base64.
impl Display for Report {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.name {
            writeln!(f, "Name: {name}")?;
        }
        let hostname = self.hostname.as_deref().unwrap_or("unavailable");
        writeln!(f, "Hostname: {hostname}")?;
        for address in &self.addresses {
            writeln!(f, "IP: {address}")?;
        }
        writeln!(f, "RemoteAddr: {}", self.remote_addr)?;
        if let Some(tls) = &self.tls {
            let verification = tls.client_verification;
            writeln!(f, "TLS: {} {} ({verification})", tls.version, tls.cipher)?;
            if let Some(alpn) = &tls.alpn {
                writeln!(f, "ALPN: {alpn}")?;
            }
            for subject in &tls.client_subjects {
                writeln!(f, "Client subject: {subject}")?;
            }
        }
        if let Some(error) = &self.server_status.reload_error {
            writeln!(f, "Reload error: {error}")?;
        }
        if let Some(environment) = &self.environment {
            writeln!(f, "Environment:")?;
            for line in environment {
                writeln!(f, "  {line}")?;
            }
        }
        writeln!(f, "{} {} {}", self.method, self.target, self.http_version)?;
        // HTTP/2 carries the host in `:authority`, so it's missing from the header map.
        if !self.headers.contains_key("host") && !self.host.is_empty() {
            writeln!(f, "  host: {}", self.host)?;
        }
        for (name, values) in &self.headers {
            for value in values {
                writeln!(f, "  {name}: {value}")?;
            }
        }
        writeln!(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        Report {
            name: Some("blue".to_owned()),
            hostname: Some("web-1".to_owned()),
            addresses: vec!["10.0.0.5".to_owned()],
            remote_addr: "10.0.0.9:4000".to_owned(),
            environment: None,
            method: "GET".to_owned(),
            target: "/a?b=c".to_owned(),
            host: "example.test".to_owned(),
            http_version: "HTTP/2.0".to_owned(),
            headers: BTreeMap::from([("accept".to_owned(), vec!["*/*".to_owned()])]),
            body: String::new(),
            body_encoding: BodyEncoding::Text,
            tls: None,
            server_status: ServerStatus::default(),
        }
    }

    #[test]
    fn text_includes_the_http2_host_missing_from_headers() {
        insta::assert_snapshot!(report(), @r"
        Name: blue
        Hostname: web-1
        IP: 10.0.0.5
        RemoteAddr: 10.0.0.9:4000
        GET /a?b=c HTTP/2.0
          host: example.test
          accept: */*
        ");
    }

    #[test]
    fn text_shows_the_negotiated_tls_session() {
        let report = Report {
            tls: Some(TlsReport {
                version: "TLSv1_3".to_owned(),
                cipher: "TLS13_AES_128_GCM_SHA256".to_owned(),
                alpn: Some("h2".to_owned()),
                certificate_source: CertificateSource::Generated,
                client_verification: ClientVerification::Unverified,
                client_subjects: vec!["CN=client".to_owned()],
            }),
            ..report()
        };

        insta::assert_snapshot!(report, @r"
        Name: blue
        Hostname: web-1
        IP: 10.0.0.5
        RemoteAddr: 10.0.0.9:4000
        TLS: TLSv1_3 TLS13_AES_128_GCM_SHA256 (unverified)
        ALPN: h2
        Client subject: CN=client
        GET /a?b=c HTTP/2.0
          host: example.test
          accept: */*
        ");
    }

    #[test]
    fn environment_lines_follow_their_heading() {
        let report = Report {
            environment: Some(vec!["A=1".to_owned()]),
            ..report()
        };

        insta::assert_snapshot!(report, @r"
        Name: blue
        Hostname: web-1
        IP: 10.0.0.5
        RemoteAddr: 10.0.0.9:4000
        Environment:
          A=1
        GET /a?b=c HTTP/2.0
          host: example.test
          accept: */*
        ");
    }

    #[test]
    fn a_rejected_reload_is_visible_in_text() {
        let report = Report {
            server_status: ServerStatus {
                reload_error: Some("bad key".to_owned()),
                degradations: Vec::new(),
            },
            ..report()
        };

        assert_eq!(
            report.to_string().lines().nth(4),
            Some("Reload error: bad key")
        );
    }

    #[test]
    fn unknown_hostname_is_stated_rather_than_omitted() {
        let report = Report {
            hostname: None,
            ..report()
        };

        assert_eq!(
            report.to_string().lines().nth(1),
            Some("Hostname: unavailable")
        );
    }
}
