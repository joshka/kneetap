//! gRPC calls on the listener that also serves HTTP.

use kneetap_api::grpc::reporter_client::ReporterClient;
use kneetap_api::grpc::{GetReportRequest, PingRequest};
use kneetap_server::{Server, ServerBuilder};
use tonic::transport::Channel;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

/// Starts a server and returns its HTTP base URL.
fn start() -> String {
    start_with(Server::builder().name("blue"))
}

/// Starts a server from `builder` on a free port and returns its HTTP base URL.
fn start_with(builder: ServerBuilder) -> String {
    let server = builder.bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.local_addr());
    tokio::spawn(server.serve(std::future::pending()));
    url
}

async fn channel(url: &str) -> Channel {
    Channel::from_shared(url.to_owned())
        .unwrap()
        .connect()
        .await
        .unwrap()
}

async fn serving_status(url: &str) -> i32 {
    let request = HealthCheckRequest {
        service: "kneetap.v1.Reporter".to_owned(),
    };
    let mut client = HealthClient::new(channel(url).await);
    client.check(request).await.unwrap().into_inner().status
}

#[tokio::test]
async fn reports_the_call_and_its_metadata() {
    let url = start();
    let mut client = ReporterClient::new(channel(&url).await);
    let mut request = tonic::Request::new(GetReportRequest {});
    let trace = "abc".parse().unwrap();
    request.metadata_mut().insert("x-trace", trace);

    let report = client.get_report(request).await.unwrap().into_inner();

    assert_eq!(report.name.as_deref(), Some("blue"));
    let trace = report.metadata.iter().find(|entry| entry.name == "x-trace");
    assert_eq!(trace.map(|entry| entry.value.as_str()), Some("abc"));
}

#[tokio::test]
async fn ping_and_http_share_the_port() {
    let url = start();
    let mut client = ReporterClient::new(channel(&url).await);

    client.ping(PingRequest {}).await.unwrap();

    // Plain reqwest in a test has no provider of its own, so a global one is fine here.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let bench = reqwest::get(format!("{url}/bench")).await.unwrap();
    assert_eq!(bench.text().await.unwrap(), "1");
}

#[tokio::test]
async fn grpc_health_follows_the_http_status() {
    let url = start();
    assert_eq!(serving_status(&url).await, ServingStatus::Serving as i32);

    // Plain reqwest in a test has no provider of its own, so a global one is fine here.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
        .post(format!("{url}/health"))
        .header("content-type", "application/json")
        .body("503")
        .send()
        .await
        .unwrap();

    assert_eq!(serving_status(&url).await, ServingStatus::NotServing as i32);
}

#[tokio::test]
async fn reports_the_peer_address() {
    let url = start();
    let mut client = ReporterClient::new(channel(&url).await);

    let response = client.get_report(GetReportRequest {}).await.unwrap();

    let report = response.into_inner();
    let peer: std::net::SocketAddr = report.remote_addr.parse().unwrap();
    assert_eq!(peer.ip().to_string(), "127.0.0.1");
}

#[tokio::test]
async fn reports_are_not_limited_by_the_request_body_size() {
    let url = start_with(Server::builder().max_body_size(1));
    let mut client = ReporterClient::new(channel(&url).await);

    let report = client.get_report(GetReportRequest {}).await;

    assert_eq!(report.map(|_| ()).map_err(|status| status.code()), Ok(()));
}

#[tokio::test]
async fn shutdown_ends_open_health_watches() {
    let server = Server::builder().bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.local_addr());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(async {
        stopped.await.ok();
    }));
    let request = HealthCheckRequest {
        service: String::new(),
    };
    let mut client = HealthClient::new(channel(&url).await);
    let mut watch = client.watch(request).await.unwrap().into_inner();
    watch.message().await.unwrap();

    stop.send(()).unwrap();

    let finished = tokio::time::timeout(std::time::Duration::from_secs(5), serving).await;
    assert_eq!(finished.map(|_| ()).map_err(|_| "still serving"), Ok(()));
}

#[tokio::test]
async fn reflection_v1_lists_the_services() {
    use tonic_reflection::pb::v1::ServerReflectionRequest;
    use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
    use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
    use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;

    let url = start();
    let request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let mut client = ServerReflectionClient::new(channel(&url).await);

    let mut replies = client
        .server_reflection_info(futures_util::stream::once(async { request }))
        .await
        .unwrap()
        .into_inner();

    let reply = replies.message().await.unwrap().unwrap().message_response;
    let Some(MessageResponse::ListServicesResponse(list)) = reply else {
        panic!("unexpected reply: {reply:?}");
    };
    let mut names: Vec<_> = list
        .service
        .into_iter()
        .map(|service| service.name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "grpc.health.v1.Health",
            "grpc.reflection.v1.ServerReflection",
            "kneetap.v1.Reporter"
        ]
    );
}

#[tokio::test]
async fn reflection_v1alpha_lists_the_services() {
    use tonic_reflection::pb::v1alpha::ServerReflectionRequest;
    use tonic_reflection::pb::v1alpha::server_reflection_client::ServerReflectionClient;
    use tonic_reflection::pb::v1alpha::server_reflection_request::MessageRequest;
    use tonic_reflection::pb::v1alpha::server_reflection_response::MessageResponse;

    let url = start();
    let request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let mut client = ServerReflectionClient::new(channel(&url).await);

    let mut replies = client
        .server_reflection_info(futures_util::stream::once(async { request }))
        .await
        .unwrap()
        .into_inner();

    let reply = replies.message().await.unwrap().unwrap().message_response;
    let Some(MessageResponse::ListServicesResponse(list)) = reply else {
        panic!("unexpected reply: {reply:?}");
    };
    let mut names: Vec<_> = list
        .service
        .into_iter()
        .map(|service| service.name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "grpc.health.v1.Health",
            "grpc.reflection.v1alpha.ServerReflection",
            "kneetap.v1.Reporter"
        ]
    );
}
