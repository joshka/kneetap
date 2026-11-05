use std::fmt;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use kneetap_api::{HEALTH_HEADER, HEALTH_MARKER};
use tokio::sync::Mutex;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use crate::body;
use crate::routes::Rejection;

/// Longest body a status update needs, since it only holds a number.
const MAX_UPDATE_SIZE: usize = 64;

/// The gRPC services whose health follows the HTTP status. The empty name means the whole server.
const GRPC_SERVICES: [&str; 2] = ["", "kneetap.v1.Reporter"];

/// The status `/health` returns, which clients change to simulate failures.
pub struct Health {
    status: AtomicU16,
    grpc: HealthReporter,

    /// Serializes changes, so that an update can't undo the status set for draining.
    update: Mutex<Update>,
}

/// State that only changes while holding [`Health::update`].
#[derive(Default)]
struct Update {
    draining: bool,
}

impl fmt::Debug for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Health").field(&self.status).finish()
    }
}

impl Health {
    /// Starts healthy, publishing the status through `grpc` as well.
    pub async fn new(grpc: HealthReporter) -> Self {
        let health = Self {
            status: AtomicU16::new(StatusCode::OK.as_u16()),
            grpc,
            update: Mutex::default(),
        };
        health.publish(ServingStatus::Serving).await;
        health
    }

    /// Reports unhealthy from now on, so that load balancers stop sending new requests.
    pub async fn drain(&self) {
        let mut update = self.update.lock().await;
        update.draining = true;
        self.status
            .store(StatusCode::SERVICE_UNAVAILABLE.as_u16(), Ordering::Relaxed);
        self.publish(ServingStatus::NotServing).await;
    }

    /// Ends open gRPC health watches, which would otherwise hold a graceful shutdown open forever.
    pub async fn stop_watches(&self) {
        let mut grpc = self.grpc.clone();
        for service in GRPC_SERVICES {
            grpc.clear_service_status(service).await;
        }
    }

    /// Sets every gRPC service to `status`, so a whole-server check agrees with a per-service one.
    async fn publish(&self, status: ServingStatus) {
        for service in GRPC_SERVICES {
            self.grpc.set_service_status(service, status).await;
        }
    }
}

/// Reports the health status, or changes it on `POST`.
///
/// Updates must be JSON because browsers can't send that type across sites without a preflight,
/// which this path refuses, so a web page can't change the status of a server its visitor can
/// reach.
pub async fn respond(
    health: &Health,
    parts: &Parts,
    body: Body,
    timeout: Duration,
) -> Result<Response, Rejection> {
    if parts.method == Method::POST {
        let status = read_update(parts, body, timeout).await?;
        let update = health.update.lock().await;
        if update.draining {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "server shutting down\n"));
        }
        health.status.store(status, Ordering::Relaxed);
        let serving = if status < 300 {
            ServingStatus::Serving
        } else {
            ServingStatus::NotServing
        };
        health.publish(serving).await;
        return Ok(Response::default());
    }
    let status = health.status.load(Ordering::Relaxed);
    let mut response = Response::default();
    *response.status_mut() = StatusCode::from_u16(status).expect("updates are validated");
    response.headers_mut().insert(
        HeaderName::from_static(HEALTH_HEADER),
        HeaderValue::from_static(HEALTH_MARKER),
    );
    // A cached status would hide the failover this endpoint exists to test.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Reads a status from an update, limited to statuses that a response can carry usefully.
async fn read_update(parts: &Parts, body: Body, timeout: Duration) -> Result<u16, Rejection> {
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .is_some_and(|media| media.type_() == mime::APPLICATION && media.subtype() == mime::JSON);
    if !is_json {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected application/json\n",
        ));
    }
    let bytes = body::read(body, MAX_UPDATE_SIZE, timeout).await?;
    let status: u16 = serde_json::from_slice(&bytes)
        .map_err(|_| (StatusCode::BAD_REQUEST, "expected an integer status\n"))?;
    if !(200..=599).contains(&status) {
        return Err((StatusCode::BAD_REQUEST, "status must be 200 through 599\n"));
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use rstest::rstest;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(1);

    async fn health() -> Health {
        Health::new(tonic_health::server::health_reporter().0).await
    }

    async fn post(health: &Health, content_type: &str, body: &'static str) -> StatusCode {
        let request = Request::post("/health")
            .header("content-type", content_type)
            .body(())
            .unwrap();
        match respond(health, &request.into_parts().0, body.into(), TIMEOUT).await {
            Ok(response) => response.status(),
            Err((status, _)) => status,
        }
    }

    async fn get(health: &Health) -> Response {
        let parts = Request::get("/health").body(()).unwrap().into_parts().0;
        respond(health, &parts, Body::empty(), TIMEOUT)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn draining_reports_unhealthy_and_refuses_updates() {
        let health = health().await;

        health.drain().await;

        let update = post(&health, "application/json", "200").await;
        assert_eq!(update, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(get(&health).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn starts_healthy_and_identifies_itself() {
        let response = get(&health().await).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[HEALTH_HEADER], HEALTH_MARKER);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn an_update_changes_later_responses() {
        let health = health().await;

        let update = post(&health, "application/json", "503").await;

        assert_eq!(update, StatusCode::OK);
        assert_eq!(get(&health).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[rstest]
    #[case("application/json", "199", StatusCode::BAD_REQUEST)]
    #[case("application/json", "600", StatusCode::BAD_REQUEST)]
    #[case("application/json", "oops", StatusCode::BAD_REQUEST)]
    #[case(
        "application/json",
        "11111111111111111111111111111111111111111111111111111111111111111",
        StatusCode::PAYLOAD_TOO_LARGE
    )]
    #[case(
        "application/x-www-form-urlencoded",
        "500",
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    )]
    #[tokio::test]
    async fn invalid_updates_leave_the_status_unchanged(
        #[case] content_type: &str,
        #[case] body: &'static str,
        #[case] refusal: StatusCode,
    ) {
        let health = health().await;

        let update = post(&health, content_type, body).await;

        assert_eq!(update, refusal);
        assert_eq!(get(&health).await.status(), StatusCode::OK);
    }
}
