//! Shutdown, exercised over real sockets.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use kneetap_server::{Error, Server, ServerBuilder, TlsConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tonic::Code;
use tonic::transport::Channel;
use tonic_reflection::pb::v1::ServerReflectionRequest;
use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;

const SETTLE: Duration = Duration::from_millis(100);

/// Longer than a test should ever take, so reaching it means a timeout failed to apply.
const LONG: Duration = Duration::from_mins(1);

/// How soon shutdown should end when nothing needs draining.
const PROMPT: Duration = Duration::from_secs(5);

struct Running {
    address: SocketAddr,
    stop: oneshot::Sender<()>,
    serving: JoinHandle<Result<(), Error>>,
}

fn start(builder: ServerBuilder) -> Running {
    let server = builder.bind("127.0.0.1:0").unwrap();
    let address = server.local_addr();
    let (stop, stopped) = oneshot::channel();
    let serving = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    Running {
        address,
        stop,
        serving,
    }
}

async fn send(address: SocketAddr, request: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn response(stream: &mut TcpStream) -> String {
    let mut text = Vec::new();
    stream.read_to_end(&mut text).await.unwrap();
    String::from_utf8_lossy(&text).into_owned()
}

#[tokio::test]
async fn a_request_in_progress_finishes_before_serving_ends() {
    let server = start(Server::builder());
    let request = "GET /?wait=300ms HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";
    let mut slow = send(server.address, request).await;
    tokio::time::sleep(SETTLE).await;

    server.stop.send(()).unwrap();

    let response = response(&mut slow).await;
    assert_eq!(response.lines().next(), Some("HTTP/1.1 200 OK"));
    server.serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn new_connections_are_refused_while_requests_drain() {
    let server = start(Server::builder());
    let request = "GET /?wait=500ms HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";
    let _slow = send(server.address, request).await;
    tokio::time::sleep(SETTLE).await;

    server.stop.send(()).unwrap();
    tokio::time::sleep(SETTLE).await;

    let refused = TcpStream::connect(server.address).await.unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);
    server.serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_shutdown_timeout_ends_work_that_would_outlast_it() {
    let builder = Server::builder().shutdown_timeout(Duration::from_millis(200));
    let server = start(builder);
    let request = "GET /?wait=1m HTTP/1.1\r\nHost: test\r\n\r\n";
    let _slow = send(server.address, request).await;
    tokio::time::sleep(SETTLE).await;
    let start = Instant::now();

    server.stop.send(()).unwrap();
    server.serving.await.unwrap().unwrap();

    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[tokio::test]
async fn a_silent_tls_handshake_does_not_delay_shutdown() {
    let server = Server::builder()
        .handshake_timeout(LONG)
        .shutdown_timeout(LONG)
        .https_listener(TcpListener::bind("127.0.0.1:0").unwrap())
        .tls(TlsConfig::generated())
        .build()
        .unwrap();
    let address = server.local_addr();
    let (stop, stopped) = oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let _silent = TcpStream::connect(address).await.unwrap();
    tokio::time::sleep(SETTLE).await;

    stop.send(()).unwrap();

    let finished = tokio::time::timeout(PROMPT, serving).await;
    finished.expect("shutdown waited").unwrap().unwrap();
}

#[tokio::test]
async fn an_idle_reflection_stream_does_not_delay_shutdown() {
    let builder = Server::builder().body_timeout(LONG).shutdown_timeout(LONG);
    let server = start(builder);
    let channel = Channel::from_shared(format!("http://{}", server.address))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let requests = stream::iter([request]).chain(stream::pending());
    let mut client = ServerReflectionClient::new(channel);
    let call = client.server_reflection_info(requests).await;
    let mut responses = call.unwrap().into_inner();
    responses.message().await.unwrap().unwrap();

    server.stop.send(()).unwrap();

    let ended = responses.message().await.unwrap_err();
    assert_eq!(ended.code(), Code::Unavailable);
    let finished = tokio::time::timeout(PROMPT, server.serving).await;
    finished.expect("shutdown waited").unwrap().unwrap();
}

#[tokio::test]
async fn websockets_are_told_the_server_is_going_away() {
    let server = start(Server::builder());
    let url = format!("ws://{}/echo", server.address);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    server.stop.send(()).unwrap();

    let Some(Ok(Message::Close(Some(frame)))) = socket.next().await else {
        panic!("expected a close frame");
    };
    assert_eq!(frame.code, CloseCode::Away);
    server.serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn serving_waits_for_a_websocket_client_to_answer_the_close() {
    let server = start(Server::builder());
    let url = format!("ws://{}/echo", server.address);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    server.stop.send(()).unwrap();
    tokio::time::sleep(SETTLE).await;

    assert!(!server.serving.is_finished());
    while socket.next().await.is_some() {}
    server.serving.await.unwrap().unwrap();
}
