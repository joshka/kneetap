use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, Version, header};
use axum::response::Response;
use itertools::Itertools;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::health::{self, Health};
use crate::identity::Identity;
use crate::status::Status;
use crate::{body, data, echo, html, report};

/// The description of this API, served at `/openapi.yaml`.
const OPENAPI: &str = include_str!("../openapi.yaml");

/// Settings and host identity that every request reads.
#[derive(Debug)]
pub struct Context {
    pub identity: Identity,
    pub max_body_size: usize,
    pub max_wait: Duration,
    pub enable_env: bool,
    pub max_data_size: u64,
    pub health: Health,
    pub allow_any_ws_origin: bool,
    pub max_ws_message_size: usize,
    pub status: Status,
    pub body_timeout: Duration,
    pub write_idle_timeout: Duration,
    pub idle_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub websockets: TaskTracker,
    pub stop: CancellationToken,
}

/// The parameters `/` and `/api` own. Any others belong to the client and are only reported.
#[derive(Debug, Deserialize)]
struct ReportQuery {
    wait: Option<String>,

    #[serde(default)]
    env: bool,
}

/// A refused request: the status and the explanation sent as its body.
pub type Rejection = (StatusCode, &'static str);

/// Builds the HTTP service.
///
/// Everything goes through one fallback rather than per-path routes, because unknown paths and any
/// method are valid input: they belong to whatever application is being tested.
pub fn router(context: Arc<Context>) -> Router {
    Router::new().fallback(handle).with_state(context)
}

/// Handles every request, removing the body from `HEAD` responses after routing so that their
/// headers match the `GET` they stand in for.
async fn handle(State(context): State<Arc<Context>>, request: Request) -> Response {
    let head_only = request.method() == Method::HEAD;
    let response = dispatch(&context, request).await;
    let mut response = response.unwrap_or_else(|(status, message)| text(status, message));
    if head_only {
        // An empty body would otherwise be announced as length 0.
        let length = response.body().size_hint().exact();
        let headers = response.headers_mut();
        if let Some(length) = length
            && !headers.contains_key(header::CONTENT_LENGTH)
        {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        }
        *response.body_mut() = Body::empty();
    }
    response
}

/// Chooses a response by path.
async fn dispatch(context: &Context, request: Request) -> Result<Response, Rejection> {
    let (mut parts, body) = request.into_parts();
    if is_grpc(&parts) {
        return Ok(tonic::Status::unimplemented("unknown method").into_http());
    }
    // HTTP/2 WebSockets start with CONNECT (RFC 8441 section 4), and the upgrade validates them.
    let h2_connect = parts.method == Method::CONNECT && parts.version == Version::HTTP_2;
    if parts.uri.path() == "/echo" && (parts.method == Method::GET || h2_connect) {
        return echo::upgrade(context, &mut parts).await;
    }
    let path = parts.uri.path();
    if let Some(allow) = disallowed(path, &parts.method) {
        return Ok(method_not_allowed(allow));
    }
    match path {
        "/bench" => Ok(text(StatusCode::OK, "1")),
        "/openapi.yaml" => Ok(openapi()),
        "/health" => health::respond(&context.health, &parts, body, context.body_timeout).await,
        "/data" => data::respond(&parts, context.max_data_size),
        "/" | "/api" => report_with_options(context, &parts, body).await,
        // Other paths belong to whatever application is being tested, so their query parameters
        // are reported, never interpreted.
        _ => {
            let bytes = read_body(context, body).await?;
            Ok(report_response(context, &parts, &bytes, None))
        }
    }
}

/// Reports a request on a path that owns `wait` and `env`, refusing bad values before any upload.
async fn report_with_options(
    context: &Context,
    parts: &Parts,
    body: Body,
) -> Result<Response, Rejection> {
    let parsed = Query::<ReportQuery>::try_from_uri(&parts.uri);
    let Query(query) = parsed.map_err(|_| (StatusCode::BAD_REQUEST, "invalid query\n"))?;
    let environment = requested_environment(context, &query)?;
    let delay = requested_wait(context, query.wait.as_deref())?;
    // Reading first sends any `100 Continue`, so a client's upload isn't held up by the delay it
    // asked for on the response.
    let bytes = read_body(context, body).await?;
    tokio::time::sleep(delay).await;
    Ok(report_response(context, parts, &bytes, environment))
}

/// Returns the environment if the client asked for it, which the operator must have allowed.
fn requested_environment(
    context: &Context,
    query: &ReportQuery,
) -> Result<Option<Vec<String>>, Rejection> {
    if !query.env {
        return Ok(None);
    }
    if !context.enable_env {
        let message = "environment disclosure is disabled\n";
        return Err((StatusCode::FORBIDDEN, message));
    }
    Ok(Some(environment_lines()))
}

/// Lists the process environment as `NAME=value` lines, sorted so reports are easy to compare.
fn environment_lines() -> Vec<String> {
    std::env::vars_os()
        .map(|(name, value)| format!("{}={}", name.display(), value.display()))
        .sorted_unstable()
        .collect_vec()
}

/// Returns the response delay clients request to test how they handle slow backends.
fn requested_wait(context: &Context, wait: Option<&str>) -> Result<Duration, Rejection> {
    let Some(wait) = wait else {
        return Ok(Duration::ZERO);
    };
    let parsed = humantime::parse_duration(wait);
    let wait = parsed.map_err(|_| (StatusCode::BAD_REQUEST, "invalid wait\n"))?;
    if wait > context.max_wait {
        return Err((StatusCode::BAD_REQUEST, "wait exceeds maximum\n"));
    }
    Ok(wait)
}

/// Reports whether an unmatched request is a gRPC call, which must get a gRPC answer.
fn is_grpc(parts: &Parts) -> bool {
    parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_grpc_type)
}

/// Matches `application/grpc` and its `+format` variants, but not `application/grpc-web`.
fn is_grpc_type(content_type: &str) -> bool {
    // `mime` refuses the whitespace that RFC 9110 section 5.6.6 allows before parameters.
    let (essence, _) = content_type.split_once(';').unwrap_or((content_type, ""));
    essence
        .trim_end()
        .parse::<mime::Mime>()
        .is_ok_and(|media| media.type_() == mime::APPLICATION && media.subtype() == "grpc")
}

fn openapi() -> Response {
    with_type(
        StatusCode::OK,
        "application/yaml",
        versioned_openapi().into(),
    )
}

/// Replaces the `CARGO_PKG_VERSION` placeholder in the API description with this crate's version.
fn versioned_openapi() -> String {
    OPENAPI.replace("CARGO_PKG_VERSION", env!("CARGO_PKG_VERSION"))
}

/// Returns the methods a path allows when it refuses this one.
fn disallowed(path: &str, method: &Method) -> Option<&'static str> {
    let allow = match path {
        "/health" => "GET, HEAD, POST",
        "/echo" => "GET",
        "/api" | "/bench" | "/data" | "/openapi.yaml" => "GET, HEAD",
        // Reports accept any method, except CONNECT, which asks for a tunnel.
        _ if method == Method::CONNECT => "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS, TRACE",
        _ => return None,
    };
    let allowed = allow.split(", ").any(|name| name == method.as_str());
    (!allowed).then_some(allow)
}

/// Collects the body up to the configured limit, reporting any read failure as too large.
async fn read_body(context: &Context, body: Body) -> Result<Bytes, Rejection> {
    body::read(body, context.max_body_size, context.body_timeout).await
}

/// Reports the request as text or JSON. Reports are never cached, because each one describes a
/// single request.
fn report_response(
    context: &Context,
    parts: &Parts,
    bytes: &[u8],
    environment: Option<Vec<String>>,
) -> Response {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .expect("the listener attaches the peer address")
        .0;
    let mut report = report::build(&context.identity, peer, environment, parts, bytes);
    report.server_status = context.status.read().clone();
    let is_api = parts.uri.path() == "/api";
    // Repeated fields mean the same as one comma-separated list (RFC 9110 section 5.3).
    let accept = parts
        .headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .join(",");
    let format = if is_api {
        Format::Json
    } else {
        Format::from_accept(&accept)
    };
    let mut response = match format {
        Format::Json => {
            let json = serde_json::to_vec(&report).expect("a report only holds strings");
            with_type(StatusCode::OK, "application/json", json.into())
        }
        Format::Html => html_response(&report),
        Format::Text => {
            let mut output = report.to_string().into_bytes();
            output.extend_from_slice(bytes);
            with_type(StatusCode::OK, TEXT, output.into())
        }
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if !is_api {
        headers.insert(header::VARY, HeaderValue::from_static("Accept"));
    }
    response
}

/// Serves the report page, which echoes untrusted input, with headers that stop browsers running
/// or reinterpreting it.
///
/// Without random bytes for a script nonce, the page is still served, just without its script.
fn html_response(report: &kneetap_api::Report) -> Response {
    let nonce = getrandom::u64()
        .inspect_err(|error| tracing::warn!(%error, "html.nonce.unavailable"))
        .ok()
        .map(|nonce| format!("{nonce:016x}"));
    let page = html::render(report, nonce.as_deref());
    let mut response = Response::new(Body::from(page));
    let headers = response.headers_mut();
    for (name, value) in html::headers(nonce.as_deref()) {
        let value = HeaderValue::from_str(&value).expect("page headers are ASCII");
        headers.insert(header::HeaderName::from_static(name), value);
    }
    response
}

/// How a report is written for the client that asked for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Text,
    Json,
    Html,
}

impl Format {
    /// Picks JSON or HTML only when the client names it and ranks it above text.
    ///
    /// Terminal clients such as curl send `*/*` and get text, while browsers name `text/html`. This
    /// never refuses a request: the server's job is to report it, whatever the client accepts.
    fn from_accept(accept: &str) -> Self {
        let (_, text) = ranking(accept, "text", "plain");
        let named = |kind, subtype| match ranking(accept, kind, subtype) {
            (3, quality) if quality > text => quality,
            _ => 0.0,
        };
        let json = named("application", "json");
        let html = named("text", "html");
        if json > 0.0 && json >= html {
            Self::Json
        } else if html > 0.0 {
            Self::Html
        } else {
            Self::Text
        }
    }
}

/// Finds the most specific `Accept` entry matching a media type, as `(specificity, quality)`.
fn ranking(accept: &str, kind: &str, subtype: &str) -> (u8, f32) {
    let mut best = (0, 0.0);
    for media in accept
        .split(',')
        .filter_map(|item| item.trim().parse::<mime::Mime>().ok())
    {
        let specificity = match (media.type_().as_str(), media.subtype().as_str()) {
            (entry, entry_subtype) if entry == kind && entry_subtype == subtype => 3,
            (entry, "*") if entry == kind => 2,
            ("*", "*") => 1,
            _ => continue,
        };
        let quality = media
            .get_param("q")
            .map_or(Some(1.0), |q| q.as_str().parse::<f32>().ok())
            .filter(|q| (0.0..=1.0).contains(q))
            .unwrap_or(0.0);
        if (specificity, quality) > best {
            best = (specificity, quality);
        }
    }
    best
}

const TEXT: &str = "text/plain; charset=utf-8";

fn text(status: StatusCode, message: &'static str) -> Response {
    with_type(status, TEXT, message.into())
}

fn method_not_allowed(allow: &'static str) -> Response {
    let mut response = text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(allow));
    response
}

fn with_type(status: StatusCode, content_type: &'static str, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::http::Request;
    use kneetap_api::{CertificateSource, ClientVerification, ConnectionReport, Report, TlsReport};
    use rstest::rstest;
    use tower::ServiceExt;

    use super::*;
    use crate::host::Host;

    const MAX_BODY_SIZE: usize = 4;

    fn host() -> Host {
        Host {
            name: Some("web-1".to_owned()),
            addresses: vec!["10.0.0.5".to_owned()],
        }
    }

    async fn context() -> Context {
        Context {
            identity: Identity::new(Some("blue".to_owned()), host(), None).unwrap(),
            max_body_size: MAX_BODY_SIZE,
            max_wait: Duration::from_secs(60),
            enable_env: false,
            max_data_size: 1000,
            health: Health::new(tonic_health::server::health_reporter().0, None).await,
            allow_any_ws_origin: false,
            max_ws_message_size: 1024,
            status: Status::default(),
            body_timeout: Duration::from_secs(5),
            write_idle_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(5),
            websockets: TaskTracker::new(),
            stop: CancellationToken::new(),
        }
    }

    async fn send(request: Request<Body>) -> Response {
        send_to(context().await, request).await
    }

    async fn send_to(context: Context, request: Request<Body>) -> Response {
        router(Arc::new(context))
            .oneshot(arrived(request))
            .await
            .unwrap()
    }

    /// Adds what the listener attaches to every request.
    fn arrived(mut request: Request<Body>) -> Request<Body> {
        let peer: SocketAddr = "10.0.0.9:4000".parse().unwrap();
        let extensions = request.extensions_mut();
        extensions.insert(ConnectInfo(peer));
        extensions.insert(ConnectionReport {
            id: "aZ3kQ9xB".to_owned(),
            request_number: 2,
            opened_at: "2026-01-01T00:00:00.000Z".to_owned(),
        });
        request
    }

    fn request(method: &str, target: &str, body: &'static str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(target)
            .body(body.into())
            .unwrap()
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Hides the lines that change with each run.
    fn redact_text(text: &str) -> String {
        let redact = |line: &str| match line.split_once(": ") {
            Some((label @ ("Version" | "Started" | "Request" | "Received"), _)) => {
                format!("{label}: [redacted]")
            }
            _ => line.to_owned(),
        };
        text.lines().map(redact).join("\n")
    }

    #[tokio::test]
    async fn text_report_ends_with_the_request_body() {
        let response = send(request("POST", "/orders?id=1", "ping")).await;

        insta::assert_snapshot!(redact_text(&body_text(response).await), @r"
        Name: blue
        Hostname: web-1
        IP: 10.0.0.5
        Version: [redacted]
        Started: [redacted]
        Request: [redacted]
        Received: [redacted]
        RemoteAddr: 10.0.0.9:4000
        Connection: aZ3kQ9xB request 2 (opened 2026-01-01T00:00:00.000Z)
        POST /orders?id=1 HTTP/1.1

        ping
        ");
    }

    #[tokio::test]
    async fn api_returns_the_report_as_json() {
        let response = send(request("GET", "/api", "")).await;

        let mut json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        for field in ["version", "startedAt", "requestId", "receivedAt"] {
            json[field] = "[redacted]".into();
        }
        insta::assert_json_snapshot!(json, @r#"
        {
          "addresses": [
            "10.0.0.5"
          ],
          "body": "",
          "bodyEncoding": "text",
          "commit": null,
          "connection": {
            "id": "aZ3kQ9xB",
            "openedAt": "2026-01-01T00:00:00.000Z",
            "requestNumber": 2
          },
          "headers": {},
          "host": "",
          "hostname": "web-1",
          "httpVersion": "HTTP/1.1",
          "method": "GET",
          "name": "blue",
          "receivedAt": "[redacted]",
          "remoteAddr": "10.0.0.9:4000",
          "requestId": "[redacted]",
          "serverStatus": {
            "degradations": [],
            "reloadError": null
          },
          "startedAt": "[redacted]",
          "target": "/api",
          "tls": null,
          "version": "[redacted]"
        }
        "#);
    }

    #[tokio::test]
    async fn each_report_has_its_own_id_and_arrival_time() {
        let app = router(Arc::new(context().await));
        let mut reports = Vec::new();
        for _ in 0..2 {
            let request = arrived(request("GET", "/api", ""));
            let response = app.clone().oneshot(request).await.unwrap();
            let report: Report = serde_json::from_str(&body_text(response).await).unwrap();
            reports.push(report);
        }

        let [first, second] = &reports[..] else {
            unreachable!()
        };
        assert_ne!(first.request_id, second.request_id);
        assert_eq!(first.started_at, second.started_at);
        // RFC 3339 timestamps in UTC with fixed precision sort as text.
        assert!(first.started_at <= first.received_at, "{first:?}");
        assert!(first.received_at <= second.received_at, "{second:?}");
        assert_eq!(first.version, env!("CARGO_PKG_VERSION"));
    }

    #[rstest]
    #[case("application/grpc", true)]
    #[case("application/grpc+proto", true)]
    #[case("application/grpc;charset=utf-8", true)]
    #[case("application/grpc ;charset=utf-8", true)]
    #[case("Application/GRPC", true)]
    #[case("application/grpc-web", false)]
    #[case("application/grpc-web+proto", false)]
    fn grpc_types_match_at_their_boundary(#[case] content_type: &str, #[case] expected: bool) {
        assert_eq!(is_grpc_type(content_type), expected);
    }

    #[tokio::test]
    async fn unknown_grpc_methods_get_a_grpc_status() {
        let call = Request::post("/other.Service/Call")
            .header("content-type", "application/grpc")
            .body(Body::empty())
            .unwrap();

        let response = send(call).await;

        assert_eq!(response.headers()["grpc-status"], "12");
    }

    #[tokio::test]
    async fn bench_returns_a_fixed_body() {
        let response = send(request("GET", "/bench", "")).await;

        assert_eq!(body_text(response).await, "1");
    }

    #[rstest]
    #[case("POST", "/api", "GET, HEAD")]
    #[case("DELETE", "/bench", "GET, HEAD")]
    #[case("OPTIONS", "/health", "GET, HEAD, POST")]
    #[tokio::test]
    async fn fixed_paths_list_their_methods_when_refusing(
        #[case] method: &str,
        #[case] path: &str,
        #[case] allow: &str,
    ) {
        let response = send(request(method, path, "")).await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], allow);
    }

    #[rstest]
    #[case("PROPFIND")]
    #[case("PURGE")]
    #[case("DELETE")]
    #[tokio::test]
    async fn other_paths_report_any_method(#[case] method: &str) {
        let response = send(request(method, "/anything", "")).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn connect_is_refused_because_reports_cannot_tunnel() {
        let response = send(request("CONNECT", "/anything", "")).await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn head_keeps_the_status_and_drops_the_body() {
        let response = send(request("HEAD", "/api", "")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "");
    }

    #[tokio::test]
    async fn head_announces_the_length_get_would_send() {
        let get = send(request("GET", "/bench", "")).await;
        let head = send(request("HEAD", "/bench", "")).await;

        assert_eq!(
            head.headers()[header::CONTENT_LENGTH],
            body_text(get).await.len().to_string()
        );
    }

    #[rstest]
    #[case("1234", StatusCode::OK)]
    #[case("12345", StatusCode::PAYLOAD_TOO_LARGE)]
    #[tokio::test]
    async fn bodies_are_limited_to_the_configured_size(
        #[case] body: &'static str,
        #[case] status: StatusCode,
    ) {
        let response = send(request("POST", "/x", body)).await;

        assert_eq!(response.status(), status);
    }

    #[rstest]
    #[case("/?env=true")]
    #[case("/api?env=true")]
    #[tokio::test]
    async fn environment_is_forbidden_unless_the_operator_enables_it(#[case] target: &str) {
        let response = send(request("GET", target, "")).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn enabled_environment_is_reported() {
        let context = Context {
            enable_env: true,
            ..context().await
        };

        let response = send_to(context, request("GET", "/api?env=true", "")).await;

        let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(json["environment"], serde_json::json!(environment_lines()));
    }

    #[test]
    fn environment_lines_are_sorted() {
        let lines = environment_lines();

        assert_eq!(lines, lines.iter().cloned().sorted_unstable().collect_vec());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_delays_the_response() {
        let start = tokio::time::Instant::now();

        let response = send(request("GET", "/?wait=30s", "")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(start.elapsed(), Duration::from_secs(30));
    }

    #[rstest]
    #[case("/?env=1")]
    #[case("/?env=")]
    #[case("/?env=%ff")]
    #[case("/?env=%zz")]
    #[case("/?env=true&env=false")]
    #[case("/?wait=")]
    #[case("/?wait=1s&wait=2s")]
    #[case("/?wait=-1s")]
    #[case("/?wait=2m")]
    #[case("/?wait=%xx")]
    #[case("/data?size=x")]
    #[case("/data?size=1001")]
    #[case("/data?attachment=1")]
    #[case("/data?size=")]
    #[case("/data?size=%xx")]
    #[case("/data?size=%FF")]
    #[case("/data?size=1&size=2")]
    #[case("/data?unit=")]
    #[case("/data?unit=%xx")]
    #[case("/data?unit=KB&unit=MB")]
    #[tokio::test]
    async fn invalid_parameters_on_owned_paths_are_refused(#[case] target: &str) {
        let response = send(request("GET", target, "")).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_parameters_are_refused_before_the_body_is_read() {
        let response = send(request("POST", "/?wait=soon", "12345")).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[rstest]
    #[case("/?other=%zz&other=1")]
    #[case("/?&&")]
    #[case("/?wait=1%20s")]
    #[case("/products?size=large")]
    #[case("/search?env=prod")]
    #[case("/x?wait=soon")]
    #[case("/x?env=true")]
    #[case("/bench?env=true")]
    #[case("/health?wait=never")]
    #[case("/x?attachment=1")]
    #[tokio::test]
    async fn parameters_a_path_does_not_own_are_ignored(#[case] target: &str) {
        let response = send(request("GET", target, "")).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn data_serves_the_requested_size() {
        let response = send(request("GET", "/data?size=16", "")).await;

        assert_eq!(body_text(response).await, "000000000000000\n");
    }

    #[tokio::test]
    async fn head_data_reports_the_length_without_a_body() {
        let response = send(request("HEAD", "/data?size=16", "")).await;

        assert_eq!(response.headers()[header::CONTENT_LENGTH], "16");
        assert_eq!(body_text(response).await, "");
    }

    #[tokio::test]
    async fn head_not_modified_keeps_the_real_length() {
        let mut request = request("HEAD", "/data?size=16", "");
        let tag = HeaderValue::from_static("\"kneetap-16\"");
        request.headers_mut().insert(header::IF_NONE_MATCH, tag);

        let response = send(request).await;

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "16");
    }

    #[tokio::test]
    async fn data_refuses_methods_other_than_get_and_head() {
        let response = send(request("POST", "/data", "")).await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], "GET, HEAD");
    }

    #[rstest]
    #[case("application/json;q=0.8, text/plain;q=0.2", Format::Json)]
    #[case("", Format::Text)]
    #[case("*/*", Format::Text)]
    #[case("application/json,text/plain", Format::Text)]
    #[case("application/xml", Format::Text)]
    #[case("text/plain;q=0,*/*;q=0", Format::Text)]
    #[case(
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        Format::Html
    )]
    #[case("text/html,text/plain", Format::Text)]
    #[case("text/*", Format::Text)]
    #[case("application/json, text/html;q=0.9", Format::Json)]
    fn json_and_html_require_an_explicit_preference(#[case] accept: &str, #[case] format: Format) {
        assert_eq!(Format::from_accept(accept), format);
    }

    #[tokio::test]
    async fn browsers_get_a_page_that_runs_only_its_own_script() {
        let mut request = request("GET", "/x", "");
        let accept = HeaderValue::from_static("text/html,*/*;q=0.8");
        request.headers_mut().insert(header::ACCEPT, accept);

        let response = send(request).await;

        let headers = response.headers();
        let policy = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        let scripts = policy
            .split("; ")
            .find(|directive| directive.starts_with("script-src"));
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(
            scripts.map(str::len),
            Some("script-src 'nonce-'".len() + 16)
        );
    }

    #[rstest]
    #[case(&["text/plain;q=0.2", "application/json;q=0.8"], "application/json")]
    #[case(&["text/plain", "application/json"], "text/plain; charset=utf-8")]
    #[tokio::test]
    async fn repeated_accept_headers_count_as_one_list(
        #[case] values: &[&str],
        #[case] content_type: &str,
    ) {
        let mut request = request("GET", "/", "");
        for value in values {
            let value = HeaderValue::from_str(value).unwrap();
            request.headers_mut().append(header::ACCEPT, value);
        }

        let response = send(request).await;

        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
    }

    #[tokio::test]
    async fn http2_connect_to_echo_reaches_the_upgrade() {
        let mut request = request("CONNECT", "/echo", "");
        *request.version_mut() = Version::HTTP_2;

        let response = send(request).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn http1_connect_to_echo_is_refused() {
        let response = send(request("CONNECT", "/echo", "")).await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    fn spec() -> serde_json::Value {
        serde_saphyr::from_str(&versioned_openapi()).unwrap()
    }

    /// Returns the field names of an object, so that schemas and reports can be compared.
    fn keys(value: &serde_json::Value) -> BTreeSet<&str> {
        let object = value.as_object().unwrap();
        object.keys().map(String::as_str).collect()
    }

    /// Follows every `$ref` in a value, returning the ones that don't resolve.
    fn broken_references(spec: &serde_json::Value, value: &serde_json::Value) -> Vec<String> {
        match value {
            serde_json::Value::Object(map) => map
                .iter()
                .flat_map(|(key, value)| broken_field_references(spec, key, value))
                .collect(),
            serde_json::Value::Array(items) => items
                .iter()
                .flat_map(|value| broken_references(spec, value))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Checks one object field, which is either a `$ref` or a value that may contain them.
    fn broken_field_references(
        spec: &serde_json::Value,
        key: &str,
        value: &serde_json::Value,
    ) -> Vec<String> {
        let Some(target) = value.as_str().filter(|_| key == "$ref") else {
            return broken_references(spec, value);
        };
        let pointer = target.trim_start_matches('#');
        let found = spec.pointer(pointer).is_some();
        (!found).then(|| target.to_owned()).into_iter().collect()
    }

    #[test]
    fn openapi_version_matches_the_crate() {
        assert_eq!(spec()["info"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn every_openapi_reference_resolves() {
        let spec = spec();

        assert_eq!(broken_references(&spec, &spec), Vec::<String>::new());
    }

    #[test]
    fn openapi_documents_exactly_the_served_paths() {
        let spec = spec();

        assert_eq!(
            keys(&spec["paths"]),
            BTreeSet::from([
                "/",
                "/api",
                "/bench",
                "/data",
                "/echo",
                "/health",
                "/openapi.yaml"
            ])
        );
    }

    #[tokio::test]
    async fn the_report_schema_matches_a_real_report() {
        let context = Context {
            enable_env: true,
            ..context().await
        };
        let response = send_to(context, request("GET", "/api?env=true", "")).await;
        let report: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        let spec = spec();
        let schemas = &spec["components"]["schemas"];

        assert_eq!(keys(&report), keys(&schemas["Report"]["properties"]));
        let status_schema = &schemas["ServerStatus"]["properties"];
        assert_eq!(keys(&report["serverStatus"]), keys(status_schema));
        let connection_schema = &schemas["Connection"]["properties"];
        assert_eq!(keys(&report["connection"]), keys(connection_schema));
    }

    #[test]
    fn the_tls_schema_matches_a_real_report() {
        let tls = TlsReport {
            version: "TLSv1_3".to_owned(),
            cipher: "TLS13_AES_128_GCM_SHA256".to_owned(),
            alpn: None,
            certificate_source: CertificateSource::Generated,
            client_verification: ClientVerification::Absent,
            client_subjects: Vec::new(),
        };
        let tls = serde_json::to_value(tls).unwrap();
        let spec = spec();
        let schema = &spec["components"]["schemas"]["Tls"]["properties"];

        assert_eq!(keys(&tls), keys(schema));
    }

    #[tokio::test]
    async fn the_description_is_served_as_yaml() {
        let response = send(request("GET", "/openapi.yaml", "")).await;

        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/yaml");
        assert_eq!(body_text(response).await, versioned_openapi());
    }
}
