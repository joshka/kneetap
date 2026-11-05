//! Shutdown, exercised over real sockets.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use kneetap_server::{Error, Server, ServerBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

const SETTLE: Duration = Duration::from_millis(100);

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
