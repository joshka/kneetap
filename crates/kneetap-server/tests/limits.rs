//! Resource limits, exercised over real sockets because they exist to survive real clients.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{Method, Request};
use futures_util::StreamExt;
use h2::client::SendRequest;
use h2::ext::Protocol;
use kneetap_server::{Server, ServerBuilder, TlsConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tonic::Code;
use tonic::transport::Channel;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;
use tonic_reflection::pb::v1::ServerReflectionRequest;
use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;

const SHORT: Duration = Duration::from_millis(200);
const PATIENCE: Duration = Duration::from_secs(5);

fn start(builder: ServerBuilder) -> SocketAddr {
    let server = builder.bind("127.0.0.1:0").unwrap();
    let address = server.local_addr();
    tokio::spawn(server.serve(std::future::pending()));
    address
}

/// Sends one request on a new connection and returns its status line.
async fn status(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = vec![0; 256];
    let read = stream.read(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response[..read]).into_owned();
    response.lines().next().unwrap_or_default().to_owned()
}

const BENCH: &str = "GET /bench HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";

/// Waits for the server to close a connection, returning whether it did.
async fn closes(stream: &mut TcpStream) -> bool {
    let mut buffer = vec![0; 4096];
    tokio::time::timeout(PATIENCE, async {
        while !matches!(stream.read(&mut buffer).await, Ok(0) | Err(_)) {}
    })
    .await
    .is_ok()
}

/// Opens an HTTP/2 connection whose frames keep being processed in the background.
async fn http2(address: SocketAddr) -> SendRequest<Bytes> {
    let stream = TcpStream::connect(address).await.unwrap();
    let (client, connection) = h2::client::handshake(stream).await.unwrap();
    tokio::spawn(connection);
    client
}

#[tokio::test]
async fn silent_http_connections_are_closed() {
    let address = start(Server::builder().header_timeout(SHORT));
    let mut silent = TcpStream::connect(address).await.unwrap();

    assert!(closes(&mut silent).await);
}

#[tokio::test]
async fn silent_tls_connections_are_closed() {
    let server = Server::builder()
        .handshake_timeout(SHORT)
        .https_listener(TcpListener::bind("127.0.0.1:0").unwrap())
        .tls(TlsConfig::generated())
        .build()
        .unwrap();
    let address = server.local_addr();
    tokio::spawn(server.serve(std::future::pending()));
    let mut silent = TcpStream::connect(address).await.unwrap();

    assert!(closes(&mut silent).await);
}

#[tokio::test]
async fn silent_connections_cannot_hold_capacity_for_long() {
    let address = start(Server::builder().max_connections(2).header_timeout(SHORT));
    let mut first = TcpStream::connect(address).await.unwrap();
    let mut second = TcpStream::connect(address).await.unwrap();
    assert!(closes(&mut first).await && closes(&mut second).await);

    assert_eq!(status(address, BENCH).await, "HTTP/1.1 200 OK");
}

#[tokio::test]
async fn connections_over_the_limit_are_closed() {
    let address = start(Server::builder().max_connections(1));
    let _held = TcpStream::connect(address).await.unwrap();
    tokio::time::sleep(SHORT).await;

    let mut refused = TcpStream::connect(address).await.unwrap();

    assert!(closes(&mut refused).await);
}

#[tokio::test]
async fn requests_over_the_limit_get_503() {
    let address = start(Server::builder().max_inflight_requests(1));
    let mut slow = TcpStream::connect(address).await.unwrap();
    slow.write_all(b"GET /?wait=1m HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(SHORT).await;

    let line = status(address, BENCH).await;

    assert_eq!(line, "HTTP/1.1 503 Service Unavailable");
}

#[tokio::test]
async fn disconnecting_clients_release_request_capacity() {
    let address = start(Server::builder().max_inflight_requests(1));
    for request in [
        "GET /?wait=1m HTTP/1.1\r\nHost: test\r\n\r\n",
        "GET /data?size=1&unit=GB HTTP/1.1\r\nHost: test\r\n\r\n",
    ] {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        tokio::time::sleep(SHORT).await;
        assert_eq!(
            status(address, BENCH).await,
            "HTTP/1.1 503 Service Unavailable"
        );

        drop(stream);

        tokio::time::sleep(SHORT).await;
        assert_eq!(status(address, BENCH).await, "HTTP/1.1 200 OK", "{request}");
    }
}

#[tokio::test]
async fn a_body_that_stalls_gets_408() {
    let address = start(Server::builder().body_timeout(SHORT));

    let line = status(
        address,
        "POST /x HTTP/1.1\r\nHost: test\r\nContent-Length: 10\r\n\r\nabc",
    )
    .await;

    assert_eq!(line, "HTTP/1.1 408 Request Timeout");
}

#[tokio::test]
async fn idle_http1_connections_are_closed_after_a_request() {
    let address = start(Server::builder().idle_timeout(SHORT));
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /bench HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();

    assert!(closes(&mut stream).await);
}

#[tokio::test]
async fn idle_http2_connections_are_closed_even_if_the_client_stops_responding() {
    let builder = Server::builder()
        .idle_timeout(SHORT)
        .write_idle_timeout(SHORT);
    let address = start(builder);
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
        .await
        .unwrap();

    assert!(closes(&mut stream).await);
}

#[tokio::test]
async fn a_partial_http2_preface_cannot_outlast_the_header_timeout() {
    let address = start(Server::builder().header_timeout(SHORT));
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(b"PRI * HTTP/2.0").await.unwrap();

    assert!(closes(&mut stream).await);
}

#[tokio::test]
async fn a_delayed_response_still_invites_the_upload_at_once() {
    let address = start(Server::builder());

    let line = status(
        address,
        "POST /?wait=1m HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n",
    )
    .await;

    assert_eq!(line, "HTTP/1.1 100 Continue");
}

#[tokio::test]
async fn http2_websockets_that_keep_sending_outlive_the_idle_timeout() {
    let builder = Server::builder()
        .idle_timeout(SHORT * 5)
        .write_idle_timeout(SHORT);
    let address = start(builder);
    let mut client = http2(address).await;
    let mut request = Request::builder()
        .method(Method::CONNECT)
        .uri(format!("http://{address}/echo"))
        .header("sec-websocket-version", "13")
        .body(())
        .unwrap();
    request
        .extensions_mut()
        .insert(Protocol::from_static("websocket"));
    let (response, mut send) = client.send_request(request, false).unwrap();
    let mut received = response.await.unwrap().into_body();

    for _ in 0..8 {
        tokio::time::sleep(SHORT).await;
        // A masked text frame holding "hi", with a mask of zero.
        let frame = Bytes::from_static(b"\x81\x82\0\0\0\0hi");
        send.send_data(frame, false).unwrap();
        let echoed = received.data().await.unwrap().unwrap();
        received
            .flow_control()
            .release_capacity(echoed.len())
            .unwrap();

        assert_eq!(echoed, Bytes::from_static(b"\x81\x02hi"));
    }
}

#[tokio::test]
async fn quiet_websockets_are_closed_at_the_idle_timeout() {
    let address = start(Server::builder().idle_timeout(SHORT));
    let url = format!("ws://{address}/echo");
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    let received = tokio::time::timeout(PATIENCE, socket.next()).await.unwrap();

    let Some(Ok(Message::Close(Some(frame)))) = received else {
        panic!("expected a close frame, got {received:?}");
    };
    assert_eq!(
        (frame.code, frame.reason.as_str()),
        (CloseCode::Away, "idle timeout")
    );
}

#[tokio::test]
async fn a_zero_window_http2_client_cannot_hold_request_capacity() {
    let builder = Server::builder()
        .max_inflight_requests(1)
        .write_idle_timeout(SHORT * 5);
    let address = start(builder);
    let stream = TcpStream::connect(address).await.unwrap();
    let handshake = h2::client::Builder::new()
        .initial_window_size(0)
        .handshake::<_, Bytes>(stream);
    let (mut client, connection) = handshake.await.unwrap();
    let answering_pings = tokio::spawn(connection);
    let request = Request::get(format!("http://{address}/data?size=1&unit=MB"))
        .body(())
        .unwrap();
    let (response, _) = client.send_request(request, true).unwrap();
    let _unread = response.await.unwrap();
    let held = status(address, BENCH).await;

    let closed = tokio::time::timeout(PATIENCE, answering_pings).await;

    assert_eq!(held, "HTTP/1.1 503 Service Unavailable");
    assert!(closed.is_ok(), "the stalled connection stayed open");
    assert_eq!(status(address, BENCH).await, "HTTP/1.1 200 OK");
}

#[tokio::test]
async fn an_http2_response_delayed_past_the_write_timeout_still_arrives() {
    let address = start(Server::builder().write_idle_timeout(SHORT));
    let mut client = http2(address).await;
    let request = Request::get(format!("http://{address}/?wait=600ms"))
        .body(())
        .unwrap();

    let (response, _) = client.send_request(request, true).unwrap();

    assert_eq!(response.await.unwrap().status(), 200);
}

#[tokio::test]
async fn a_slow_http2_download_that_keeps_reading_finishes() {
    let address = start(Server::builder().write_idle_timeout(SHORT * 2));
    let mut client = http2(address).await;
    let request = Request::get(format!("http://{address}/data?size=256&unit=KiB"))
        .body(())
        .unwrap();
    let (response, _) = client.send_request(request, true).unwrap();
    let mut body = response.await.unwrap().into_body();
    let mut received = 0;

    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        received += chunk.len();
        tokio::time::sleep(SHORT / 20).await;
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }

    assert_eq!(received, 256 * 1024);
}

#[tokio::test]
async fn health_watches_that_go_quiet_release_request_capacity() {
    let address = start(
        Server::builder()
            .max_inflight_requests(1)
            .idle_timeout(SHORT),
    );
    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let request = HealthCheckRequest {
        service: String::new(),
    };
    let mut watch = HealthClient::new(channel)
        .watch(request)
        .await
        .unwrap()
        .into_inner();
    watch.message().await.unwrap().unwrap();

    let ended = tokio::time::timeout(PATIENCE, watch.message())
        .await
        .unwrap();

    assert_eq!(ended.unwrap_err().code(), Code::Unavailable);
    tokio::time::sleep(SHORT / 2).await;
    assert_eq!(status(address, BENCH).await, "HTTP/1.1 200 OK");
}

#[tokio::test]
async fn an_incomplete_http2_header_block_cannot_outlast_the_header_timeout() {
    let address = start(Server::builder().header_timeout(SHORT));
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
        .await
        .unwrap();

    // A HEADERS frame for stream 1 that promises ten bytes and delivers one.
    stream
        .write_all(b"\0\0\x0a\x01\x05\0\0\0\x01\x82")
        .await
        .unwrap();

    assert!(closes(&mut stream).await);
}

#[tokio::test]
async fn idle_http2_connections_outlast_the_header_timeout() {
    let address = start(Server::builder().header_timeout(SHORT));
    let mut client = http2(address).await;
    tokio::time::sleep(SHORT * 3).await;

    let request = Request::get(format!("http://{address}/bench"))
        .body(())
        .unwrap();
    let (response, _) = client.send_request(request, true).unwrap();

    assert_eq!(response.await.unwrap().status(), 200);
}

#[tokio::test]
async fn stalled_grpc_streams_release_request_capacity_without_a_content_type() {
    let builder = Server::builder()
        .max_inflight_requests(1)
        .body_timeout(SHORT);
    let address = start(builder);
    let mut client = http2(address).await;
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "http://{address}/grpc.reflection.v1.ServerReflection/ServerReflectionInfo"
        ))
        .body(())
        .unwrap();
    let (response, _open) = client.send_request(request, false).unwrap();
    let mut body = response.await.unwrap().into_body();
    let ended =
        tokio::time::timeout(PATIENCE, async { while body.data().await.is_some() {} }).await;
    tokio::time::sleep(SHORT).await;

    assert_eq!(ended, Ok(()));
    assert_eq!(status(address, BENCH).await, "HTTP/1.1 200 OK");
}

#[tokio::test]
async fn grpc_streams_that_keep_sending_outlive_the_body_timeout() {
    let address = start(Server::builder().body_timeout(SHORT));
    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let requests = futures_util::stream::iter(0..4).then(|_| async {
        tokio::time::sleep(SHORT / 2).await;
        ServerReflectionRequest {
            host: String::new(),
            message_request: Some(MessageRequest::ListServices(String::new())),
        }
    });

    let responses = ServerReflectionClient::new(channel)
        .server_reflection_info(requests)
        .await
        .unwrap()
        .into_inner()
        .collect::<Vec<_>>()
        .await;

    let succeeded = responses.iter().filter(|response| response.is_ok()).count();
    assert_eq!((succeeded, responses.len()), (4, 4));
}
