use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{self, ConnectInfo, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{self, IntoResponse};
use kneetap_api::ConnectionReport;
use kneetap_api::grpc::reporter_server::{Reporter, ReporterServer};
use kneetap_api::grpc::{
    Connection, FILE_DESCRIPTOR_SET, GetReportRequest, GetReportResponse, MetadataEntry,
    PingRequest, PingResponse,
};
use tokio_util::sync::CancellationToken;
use tonic::metadata::KeyAndValueRef;
use tonic::service::Routes;
use tonic::{Request, Response, Status};
use tonic_health::pb::health_server::{Health as HealthApi, HealthServer};
use tonic_reflection::server::Builder as Reflection;

use crate::identity::{self, Arrival};
use crate::limits::{DeadlineBody, QuietBody};
use crate::report::escape;
use crate::routes::Context;

/// Builds the gRPC routes, which share a listener with HTTP by matching on their paths.
///
/// These paths are gRPC's whatever the content type, so they never produce an HTTP report.
///
/// Reflection is included so that tools can call the server without a copy of the proto, in both
/// versions because tools differ in which they use.
pub fn router(context: Arc<Context>, health: HealthServer<impl HealthApi>) -> Router {
    let limit = context.max_body_size;
    let bounds = Bounds {
        body_timeout: context.body_timeout,
        idle_timeout: context.idle_timeout,
        stop: context.stop.clone(),
    };
    let reflection_v1 = reflection().build_v1();
    let reflection_v1 = reflection_v1.expect("the embedded descriptor is valid");
    let reflection_alpha = reflection().build_v1alpha();
    let reflection_alpha = reflection_alpha.expect("the embedded descriptor is valid");
    let reporter = ReporterServer::new(Service(context)).max_decoding_message_size(limit);
    Routes::new(reporter)
        .add_service(health.max_decoding_message_size(limit))
        .add_service(reflection_v1.max_decoding_message_size(limit))
        .add_service(reflection_alpha.max_decoding_message_size(limit))
        .into_axum_router()
        .layer(middleware::from_fn_with_state(bounds, bound_call))
}

/// What limits a call's streams.
#[derive(Clone)]
struct Bounds {
    body_timeout: Duration,
    idle_timeout: Duration,
    stop: CancellationToken,
}

/// Bounds both of a call's streams, because tonic reads and writes their bodies itself.
///
/// This sits on the routes rather than matching a content type, which tonic doesn't require.
async fn bound_call(
    State(bounds): State<Bounds>,
    request: extract::Request,
    next: Next,
) -> response::Response {
    let Bounds {
        body_timeout,
        idle_timeout,
        stop,
    } = bounds;
    let ended = CancellationToken::new();
    let reading = ended.clone();
    let request = request.map(|body| Body::new(DeadlineBody::new(body, body_timeout, reading)));
    let call = tokio::time::timeout(body_timeout, next.run(request)).await;
    let Ok(response) = call else {
        return StatusCode::REQUEST_TIMEOUT.into_response();
    };
    response.map(|body| Body::new(QuietBody::new(body, idle_timeout, stop, ended)))
}

/// Starts a reflection service describing the embedded package and the health service.
fn reflection() -> Reflection<'static> {
    Reflection::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
}

/// The `kneetap.v1.Reporter` service.
struct Service(Arc<Context>);

#[tonic::async_trait]
impl Reporter for Service {
    async fn get_report(
        &self,
        request: Request<GetReportRequest>,
    ) -> Result<Response<GetReportResponse>, Status> {
        let peer = request.extensions().get::<ConnectInfo<SocketAddr>>();
        let remote_addr = peer.map_or_default(|info| info.0.to_string());
        let arrival = request.extensions().get::<Arrival>();
        let arrival = arrival.expect("the listener attaches the arrival");
        let identity = &self.0.identity;
        Ok(Response::new(GetReportResponse {
            name: identity.name.clone(),
            hostname: identity.host.name.clone(),
            addresses: identity.host.addresses.clone(),
            remote_addr,
            metadata: metadata(&request),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            commit: identity.commit(),
            started_at: identity.started_at(),
            request_id: arrival.request_id.clone(),
            received_at: identity::timestamp(arrival.at),
            connection: request
                .extensions()
                .get::<ConnectionReport>()
                .map(connection),
        }))
    }

    async fn ping(&self, _: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        Ok(Response::new(PingResponse {}))
    }
}

/// Converts the connection the listener attached into its gRPC form.
fn connection(report: &ConnectionReport) -> Connection {
    Connection {
        id: report.id.clone(),
        request_number: report.request_number,
        opened_at: report.opened_at.clone(),
    }
}

/// Lists a call's metadata. Binary entries are base64 on the wire and are shown that way.
fn metadata<T>(request: &Request<T>) -> Vec<MetadataEntry> {
    request
        .metadata()
        .iter()
        .map(|item| match item {
            KeyAndValueRef::Ascii(name, value) => entry(name.as_str(), value.as_encoded_bytes()),
            KeyAndValueRef::Binary(name, value) => entry(name.as_str(), value.as_encoded_bytes()),
        })
        .collect()
}

/// Builds one metadata entry, escaping a value that isn't UTF-8.
fn entry(name: &str, value: &[u8]) -> MetadataEntry {
    MetadataEntry {
        name: name.to_owned(),
        value: escape(value),
    }
}
