//! The embedding API, exercised over a real socket.

use kneetap_server::Server;
use tokio::sync::oneshot;

#[tokio::test]
async fn serves_until_shutdown_completes() {
    let server = Server::builder().bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/bench", server.local_addr());
    let (stop, stopped) = oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));

    let body = reqwest::get(&url).await.unwrap().text().await.unwrap();
    stop.send(()).unwrap();

    assert_eq!(body, "1");
    serving.await.unwrap().unwrap();
}

#[test]
fn reports_a_bind_failure_as_such() {
    let first = Server::builder().bind("127.0.0.1:0").unwrap();

    let error = Server::builder().bind(first.local_addr()).unwrap_err();

    assert!(error.is_bind(), "{error:?}");
}
