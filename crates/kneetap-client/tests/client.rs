//! The client API against real servers.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use kneetap_client::Client;
use kneetap_server::Server;
use rstest::rstest;

/// Starts a kneetap server and returns its address.
fn kneetap(name: &str) -> SocketAddr {
    let server = Server::builder().name(name).bind("127.0.0.1:0").unwrap();
    let address = server.local_addr();
    tokio::spawn(server.serve(std::future::pending()));
    address
}

/// Starts a service unrelated to kneetap that answers every request with `status`.
async fn unrelated(status: StatusCode) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/health", listener.local_addr().unwrap());
    let app = Router::new().fallback(move || async move { status });
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

/// Starts a kneetap server reporting `status` from its health endpoint.
async fn with_health(status: u16) -> String {
    named("health", status).await
}

/// Changes what the kneetap health endpoint at `url` reports.
async fn set_health(url: &str, status: u16) {
    // Plain reqwest in a test has no provider of its own, so a global one is fine here.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .body(status.to_string())
        .send()
        .await
        .unwrap();
}

/// Starts a kneetap server called `name` that reports `status` from its health endpoint.
async fn named(name: &str, status: u16) -> String {
    let url = format!("http://{}/health", kneetap(name));
    set_health(&url, status).await;
    url
}

/// Accepts connections and never answers, returning the listener to keep it open.
async fn silent() -> (tokio::net::TcpListener, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/health", listener.local_addr().unwrap());
    (listener, url)
}

fn client() -> Client {
    Client::builder().build().unwrap()
}

#[tokio::test]
async fn reads_a_report() {
    let address = kneetap("blue");

    let report = client()
        .report(&format!("http://{address}/api"))
        .await
        .unwrap();

    assert_eq!(report.name.as_deref(), Some("blue"));
}

#[tokio::test]
async fn an_unhealthy_server_fails_its_check() {
    let url = with_health(503).await;

    let error = client().check_health(&url).await.unwrap_err();

    assert_eq!(error.unhealthy_status(), Some(503));
}

#[tokio::test]
async fn explicit_checks_accept_any_service() {
    let url = unrelated(StatusCode::OK).await;

    client().check_health(&url).await.unwrap();
}

#[rstest]
#[case(StatusCode::OK)]
#[case(StatusCode::NOT_FOUND)]
#[case(StatusCode::BAD_GATEWAY)]
#[tokio::test]
async fn discovery_skips_unrelated_services(#[case] status: StatusCode) {
    let candidates = [unrelated(status).await, with_health(200).await];

    client().discover_health(&candidates, None).await.unwrap();
}

#[tokio::test]
async fn discovery_reports_an_unhealthy_server_behind_an_unrelated_one() {
    let candidates = [unrelated(StatusCode::OK).await, with_health(503).await];

    let error = client()
        .discover_health(&candidates, None)
        .await
        .unwrap_err();

    assert_eq!(error.unhealthy_status(), Some(503));
}

#[tokio::test]
async fn an_unhealthy_server_is_not_masked_by_a_healthy_one() {
    let candidates = [with_health(503).await, with_health(200).await];

    let error = client()
        .discover_health(&candidates, None)
        .await
        .unwrap_err();

    assert_eq!(error.unhealthy_status(), Some(503));
}

#[tokio::test]
async fn discovery_fails_when_only_other_services_answer() {
    let candidates = [unrelated(StatusCode::OK).await];

    let error = client()
        .discover_health(&candidates, None)
        .await
        .unwrap_err();

    assert!(error.is_unrelated(), "{error:?}");
}

#[tokio::test]
async fn a_healthy_server_with_another_name_does_not_mask_the_target() {
    let candidates = [named("other", 200).await, named("target", 503).await];

    let error = client()
        .discover_health(&candidates, Some("target"))
        .await
        .unwrap_err();

    assert_eq!(error.unhealthy_status(), Some(503));
}

#[tokio::test]
async fn an_unhealthy_server_with_another_name_is_ignored() {
    let candidates = [named("other", 503).await, named("target", 200).await];

    client()
        .discover_health(&candidates, Some("target"))
        .await
        .unwrap();
}

#[tokio::test]
async fn discovery_fails_when_only_other_names_answer() {
    let candidates = [named("other", 200).await];

    let error = client()
        .discover_health(&candidates, Some("target"))
        .await
        .unwrap_err();

    assert!(error.is_unrelated(), "{error:?}");
}

#[tokio::test]
async fn a_silent_service_does_not_use_up_the_deadline() {
    let (_listener, silent) = silent().await;
    let candidates = [silent, with_health(200).await];
    let started = Instant::now();

    client().discover_health(&candidates, None).await.unwrap();

    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

#[tokio::test]
async fn an_unhealthy_server_is_found_behind_a_silent_service() {
    let (_listener, silent) = silent().await;
    let candidates = [silent, with_health(503).await];

    let error = client()
        .discover_health(&candidates, None)
        .await
        .unwrap_err();

    assert_eq!(error.unhealthy_status(), Some(503));
}

#[tokio::test]
async fn an_oversized_report_is_refused_without_being_decoded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap());
    let app = Router::new().fallback(|| async { vec![b' '; 9 << 20] });
    tokio::spawn(async move { axum::serve(listener, app).await });

    let error = client().report(&url).await.unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("{url} returned too much data for a report")
    );
}
