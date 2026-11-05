use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{self, Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;

use crate::host::Host;
use crate::report;

/// Settings and host identity that every request reads.
#[derive(Debug)]
pub struct Context {
    pub name: Option<String>,
    pub host: Host,
    pub max_body_size: usize,
}

/// Builds the service.
///
/// Everything goes through one fallback rather than per-path routes, because unknown paths and any
/// method are valid input: they belong to whatever application is being tested.
pub fn router(context: Arc<Context>) -> Router {
    Router::new().fallback(handle).with_state(context)
}

async fn handle(State(context): State<Arc<Context>>, request: Request) -> Response {
    let head_only = request.method() == Method::HEAD;
    let mut response = dispatch(&context, request).await;
    if head_only {
        *response.body_mut() = Body::empty();
    }
    response
}

async fn dispatch(context: &Context, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    if let Some(allow) = disallowed(path, &parts.method) {
        return method_not_allowed(allow);
    }
    match path {
        "/bench" => text(StatusCode::OK, "1"),
        _ => match body::to_bytes(body, context.max_body_size).await {
            Ok(bytes) => report_response(context, &parts, &bytes),
            Err(_) => text(StatusCode::PAYLOAD_TOO_LARGE, "body exceeds maximum\n"),
        },
    }
}

/// Returns the methods a path allows when it refuses this one.
fn disallowed(path: &str, method: &Method) -> Option<&'static str> {
    let allow = match path {
        "/api" | "/bench" => "GET, HEAD",
        // Reports accept any method, except CONNECT, which asks for a tunnel.
        _ if method == Method::CONNECT => "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS, TRACE",
        _ => return None,
    };
    let allowed =
        method != Method::CONNECT && allow.split(", ").any(|name| name == method.as_str());
    (!allowed).then_some(allow)
}

fn report_response(context: &Context, parts: &Parts, bytes: &Bytes) -> Response {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .expect("the listener attaches the peer address")
        .0;
    let report = report::build(context.name.clone(), &context.host, peer, parts, bytes);
    let is_api = parts.uri.path() == "/api";
    // Repeated fields mean the same as one comma-separated list (RFC 9110 section 5.3).
    let accept: Vec<_> = parts
        .headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    let accept = (!accept.is_empty()).then(|| accept.join(","));
    let mut response = if is_api || prefers_json(accept.as_deref()) {
        let json = serde_json::to_vec(&report).expect("a report only holds strings");
        with_type(StatusCode::OK, "application/json", json.into())
    } else {
        let mut output = report.to_string().into_bytes();
        output.extend_from_slice(bytes);
        with_type(StatusCode::OK, TEXT, output.into())
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if !is_api {
        headers.insert(header::VARY, HeaderValue::from_static("Accept"));
    }
    response
}

/// Selects JSON only when the client ranks it above text.
///
/// This never refuses a request: the server's job is to report it, whatever the client accepts.
fn prefers_json(accept: Option<&str>) -> bool {
    accept.is_some_and(|accept| {
        let (specificity, json) = ranking(accept, "application", "json");
        let (_, text) = ranking(accept, "text", "plain");
        specificity == 3 && json > 0.0 && json > text
    })
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
    use axum::http::Request;
    use rstest::rstest;
    use tower::ServiceExt;

    use super::*;

    const MAX_BODY_SIZE: usize = 4;

    async fn send(mut request: Request<Body>) -> Response {
        let peer: SocketAddr = "10.0.0.9:4000".parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        let context = Context {
            name: Some("blue".to_owned()),
            host: Host {
                name: Some("web-1".to_owned()),
                addresses: vec!["10.0.0.5".to_owned()],
            },
            max_body_size: MAX_BODY_SIZE,
        };
        router(Arc::new(context)).oneshot(request).await.unwrap()
    }

    fn request(method: &str, target: &str, body: &'static str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(target)
            .body(body.into())
            .unwrap()
    }

    async fn body_text(response: Response) -> String {
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn text_report_ends_with_the_request_body() {
        let response = send(request("POST", "/orders?id=1", "ping")).await;

        insta::assert_snapshot!(body_text(response).await, @r"
        Name: blue
        Hostname: web-1
        IP: 10.0.0.5
        RemoteAddr: 10.0.0.9:4000
        POST /orders?id=1 HTTP/1.1

        ping
        ");
    }

    #[tokio::test]
    async fn api_returns_the_report_as_json() {
        let response = send(request("GET", "/api", "")).await;

        let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        insta::assert_json_snapshot!(json, @r#"
        {
          "addresses": [
            "10.0.0.5"
          ],
          "body": "",
          "bodyEncoding": "text",
          "headers": {},
          "host": "",
          "hostname": "web-1",
          "httpVersion": "HTTP/1.1",
          "method": "GET",
          "name": "blue",
          "remoteAddr": "10.0.0.9:4000",
          "target": "/api"
        }
        "#);
    }

    #[tokio::test]
    async fn bench_returns_a_fixed_body() {
        let response = send(request("GET", "/bench", "")).await;

        assert_eq!(body_text(response).await, "1");
    }

    #[rstest]
    #[case("POST", "/api", "GET, HEAD")]
    #[case("DELETE", "/bench", "GET, HEAD")]
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
    async fn head_keeps_the_status_and_drops_the_body() {
        let response = send(request("HEAD", "/api", "")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "");
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
    #[case(Some("application/json;q=0.8, text/plain;q=0.2"), true)]
    #[case(None, false)]
    #[case(Some("*/*"), false)]
    #[case(Some("application/json,text/plain"), false)]
    #[case(Some("application/xml"), false)]
    #[case(Some("text/plain;q=0,*/*;q=0"), false)]
    fn json_requires_an_explicit_preference(#[case] accept: Option<&str>, #[case] json: bool) {
        assert_eq!(prefers_json(accept), json);
    }
}
