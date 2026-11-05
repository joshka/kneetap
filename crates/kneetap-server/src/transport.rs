use std::future::Future;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, Request};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use kneetap_api::TlsReport;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::reload::Certificates;
use crate::tls::Material;

/// How long to wait after a failed accept, so running out of file descriptors can't spin the loop.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// A bound socket and whether it speaks TLS.
#[derive(Debug)]
pub struct Listener {
    socket: TcpListener,
    secure: bool,
}

impl Listener {
    /// Wraps a socket that serves plain HTTP.
    pub const fn http(socket: TcpListener) -> Self {
        Self {
            socket,
            secure: false,
        }
    }

    /// Wraps a socket that completes a TLS handshake before serving HTTP.
    pub const fn https(socket: TcpListener) -> Self {
        Self {
            socket,
            secure: true,
        }
    }

    /// Returns the bound address, which reveals the port when binding to port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.socket
            .local_addr()
            .expect("a bound socket has an address")
    }

    /// Returns the URL clients reach this listener on.
    pub fn url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{scheme}://{}", self.local_addr())
    }
}

/// Accepts connections on every listener until `shutdown` completes.
///
/// Each connection is served directly so that handlers can see facts only it knows, such as the
/// client certificate from the handshake.
pub async fn run<F>(
    listeners: Vec<Listener>,
    app: Router,
    tls: Option<Arc<Certificates>>,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let stop = CancellationToken::new();
    let mut tasks = JoinSet::new();
    for listener in listeners {
        listener.socket.set_nonblocking(true)?;
        let socket = tokio::net::TcpListener::from_std(listener.socket)?;
        let tls = listener.secure.then(|| tls.clone()).flatten();
        tasks.spawn(accept(socket, app.clone(), tls, stop.clone()));
    }
    shutdown.await;
    stop.cancel();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Runs one listener, then waits for the connections it accepted to finish.
async fn accept(
    socket: tokio::net::TcpListener,
    app: Router,
    tls: Option<Arc<Certificates>>,
    stop: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = socket.accept() => accepted,
            () = stop.cancelled() => break,
        };
        let Ok((stream, peer)) = accepted else {
            tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
            continue;
        };
        // A handshake uses the certificates current when its connection arrived.
        let material = tls.as_ref().map(|tls| tls.current());
        connections.spawn(connection(
            stream,
            peer,
            app.clone(),
            material,
            stop.clone(),
        ));
        while connections.try_join_next().is_some() {}
    }
    while connections.join_next().await.is_some() {}
}

/// Completes the handshake, if any, then serves requests. Failures only affect this client.
async fn connection(
    stream: TcpStream,
    peer: SocketAddr,
    app: Router,
    tls: Option<Arc<Material>>,
    stop: CancellationToken,
) {
    let Some(material) = tls else {
        serve(stream, peer, None, app, stop).await;
        return;
    };
    let acceptor = TlsAcceptor::from(material.config.clone());
    let Ok(stream) = acceptor.accept(stream).await else {
        return;
    };
    let report = material.describe(stream.get_ref().1);
    serve(stream, peer, Some(report), app, stop).await;
}

/// Serves requests on one connection, which finishes those in flight once `stop` is cancelled.
async fn serve<I>(
    io: I,
    peer: SocketAddr,
    tls: Option<TlsReport>,
    app: Router,
    stop: CancellationToken,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = tower::service_fn(move |mut request: Request<Incoming>| {
        let extensions = request.extensions_mut();
        extensions.insert(ConnectInfo(peer));
        if let Some(tls) = &tls {
            extensions.insert(tls.clone());
        }
        app.clone().oneshot(request)
    });
    let mut builder = Builder::new(TokioExecutor::new());
    // HTTP/2 clients can only open WebSockets if the server advertises extended CONNECT.
    builder.http2().enable_connect_protocol();
    let connection =
        builder.serve_connection_with_upgrades(TokioIo::new(io), TowerToHyperService::new(service));
    tokio::pin!(connection);
    tokio::select! {
        _ = &mut connection => {}
        () = stop.cancelled() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}
