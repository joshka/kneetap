//! Connection details in reports, over real sockets because they describe real connections.

use std::net::SocketAddr;

use http_body_util::BodyExt;
use hyper::Request;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use kneetap_api::{ConnectionReport, Report};
use kneetap_server::Server;
use tokio::net::TcpStream;

fn start() -> SocketAddr {
    let server = Server::builder().bind("127.0.0.1:0").unwrap();
    let address = server.local_addr();
    tokio::spawn(server.serve(std::future::pending()));
    address
}

fn api(address: SocketAddr) -> Request<String> {
    Request::get(format!("http://{address}/api"))
        .header("host", address.to_string())
        .body(String::new())
        .unwrap()
}

async fn connect(address: SocketAddr) -> TokioIo<TcpStream> {
    TokioIo::new(TcpStream::connect(address).await.unwrap())
}

/// Opens an HTTP/1.1 connection whose frames keep being processed in the background.
async fn http1(address: SocketAddr) -> http1::SendRequest<String> {
    let (sender, connection) = http1::handshake(connect(address).await).await.unwrap();
    tokio::spawn(connection);
    sender
}

async fn connection_of(response: hyper::Response<hyper::body::Incoming>) -> ConnectionReport {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Report>(&body).unwrap().connection
}

#[tokio::test]
async fn requests_on_a_kept_alive_connection_share_it_and_are_numbered() {
    let address = start();
    let mut sender = http1(address).await;

    let first = connection_of(sender.send_request(api(address)).await.unwrap()).await;
    let second = connection_of(sender.send_request(api(address)).await.unwrap()).await;

    let expected = ConnectionReport {
        request_number: 2,
        ..first.clone()
    };
    assert_eq!((first.request_number, second), (1, expected));
}

#[tokio::test]
async fn each_connection_gets_a_new_id() {
    let address = start();
    let mut first = http1(address).await;
    let mut second = http1(address).await;

    let first = connection_of(first.send_request(api(address)).await.unwrap()).await;
    let second = connection_of(second.send_request(api(address)).await.unwrap()).await;

    assert_ne!(first.id, second.id);
}

#[tokio::test]
async fn concurrent_http2_requests_share_a_connection_with_distinct_numbers() {
    let address = start();
    let (mut sender, connection) = http2::handshake(TokioExecutor::new(), connect(address).await)
        .await
        .unwrap();
    tokio::spawn(connection);

    let first = sender.send_request(api(address));
    let second = sender.clone().send_request(api(address));
    let (first, second) = tokio::join!(first, second);
    let first = connection_of(first.unwrap()).await;
    let second = connection_of(second.unwrap()).await;

    let mut numbers = [first.request_number, second.request_number];
    numbers.sort_unstable();
    assert_eq!(first.id, second.id);
    assert_eq!(numbers, [1, 2]);
}
