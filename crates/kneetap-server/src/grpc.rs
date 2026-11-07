use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{self, ConnectInfo, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{self, IntoResponse};
use kneetap_api::grpc::reporter_server::{Reporter, ReporterServer};
use kneetap_api::grpc::{
    FILE_DESCRIPTOR_SET, GetReportRequest, GetReportResponse, MetadataEntry, PingRequest,
    PingResponse,
};
use tonic::metadata::KeyAndValueRef;
use tonic::service::Routes;
use tonic::{Request, Response, Status};
use tonic_health::pb::health_server::{Health as HealthApi, HealthServer};
use tonic_reflection::server::Builder as Reflection;

use crate::limits::DeadlineBody;
use crate::report::escape;
use crate::routes::Context;

/// Builds the gRPC routes, which share a listener with HTTP by matching on their paths.
///
/// Reflection is included so that tools can call the server without a copy of the proto, in both
/// versions because tools differ in which they use.
pub fn router(context: Arc<Context>, health: HealthServer<impl HealthApi>) -> Router {
    let limit = context.max_body_size;
    let timeout = context.body_timeout;
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
        .layer(middleware::from_fn_with_state(timeout, bound_body))
}

/// Applies the body timeout to a call, because tonic reads request bodies itself.
///
/// This sits on the routes rather than matching a content type, which tonic doesn't require.
async fn bound_body(
    State(timeout): State<Duration>,
    request: extract::Request,
    next: Next,
) -> response::Response {
    let request = request.map(|body| Body::new(DeadlineBody::new(body, timeout)));
    let call = tokio::time::timeout(timeout, next.run(request));
    call.await
        .unwrap_or_else(|_| StatusCode::REQUEST_TIMEOUT.into_response())
}

/// Starts a reflection service describing the embedded package.
fn reflection() -> Reflection<'static> {
    Reflection::configure().register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
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
        Ok(Response::new(GetReportResponse {
            name: self.0.name.clone(),
            hostname: self.0.host.name.clone(),
            addresses: self.0.host.addresses.clone(),
            remote_addr,
            metadata: metadata(&request),
        }))
    }

    async fn ping(&self, _: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        Ok(Response::new(PingResponse {}))
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
