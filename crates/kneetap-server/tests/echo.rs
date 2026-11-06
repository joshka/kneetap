//! WebSocket echo over a real socket.

use futures_util::{SinkExt, StreamExt};
use kneetap_server::{Server, ServerBuilder};
use rstest::rstest;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::{Error, Message};

fn start(builder: ServerBuilder) -> String {
    let server = builder.bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/echo", server.local_addr());
    tokio::spawn(server.serve(std::future::pending()));
    url
}

#[rstest]
#[case(Message::text("hello"))]
#[case(Message::binary(vec![0, 255, 1]))]
#[tokio::test]
async fn messages_come_back_unchanged(#[case] message: Message) {
    let url = start(Server::builder());
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    socket.send(message.clone()).await.unwrap();

    assert_eq!(socket.next().await.unwrap().unwrap(), message);
}

#[tokio::test]
async fn closing_completes_the_handshake() {
    let url = start(Server::builder());
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    socket.close(None).await.unwrap();

    assert_eq!(socket.next().await.unwrap().unwrap(), Message::Close(None));
}

async fn connect_from(url: &str, origin: &str) -> Result<(), Error> {
    let mut request = url.into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert("host", "app.test".parse().unwrap());
    headers.insert("origin", origin.parse().unwrap());
    tokio_tungstenite::connect_async(request).await.map(|_| ())
}

#[tokio::test]
async fn an_https_origin_works_behind_a_tls_proxy() {
    let url = start(Server::builder());

    connect_from(&url, "https://app.test").await.unwrap();
}

#[tokio::test]
async fn other_sites_are_refused() {
    let url = start(Server::builder());

    let error = connect_from(&url, "https://evil.test").await.unwrap_err();

    let Error::Http(response) = error else {
        panic!("expected an HTTP refusal, got {error:?}");
    };
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn the_operator_can_allow_any_origin() {
    let url = start(Server::builder().allow_any_ws_origin());

    connect_from(&url, "https://evil.test").await.unwrap();
}

#[tokio::test]
async fn oversized_messages_end_the_connection() {
    let url = start(Server::builder().max_ws_message_size(4));
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    socket.send(Message::text("12345")).await.unwrap();

    let reply = socket.next().await;
    let Some(Err(Error::Protocol(error))) = reply else {
        panic!("expected the connection to drop, got {reply:?}");
    };
    assert_eq!(error, ProtocolError::ResetWithoutClosingHandshake);
}
