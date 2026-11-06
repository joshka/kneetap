use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::response::Response;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use kneetap_api::TlsReport;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::limits::{Activity, Deadlines, Limits, TrackedBody};
use crate::reload::Certificates;
use crate::tls::Material;

/// Shortest wait after a failed accept, so running out of file descriptors can't spin the loop.
const MIN_ACCEPT_BACKOFF: Duration = Duration::from_millis(10);

/// Longest wait after repeated failed accepts, so recovery stays prompt.
const MAX_ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

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

/// What every connection on a server shares.
#[derive(Clone)]
struct Shared {
    app: Router,
    limits: Limits,
    connections: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    stop: CancellationToken,
}

/// Accepts connections on every listener until `shutdown` completes.
///
/// Each connection is served directly so that handlers can see facts only it knows, such as the
/// client certificate from the handshake, and so limits can own each connection's lifetime.
pub async fn run<F>(
    listeners: Vec<Listener>,
    app: Router,
    tls: Option<Arc<Certificates>>,
    limits: Limits,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let shared = Shared {
        app,
        limits,
        connections: Arc::new(Semaphore::new(limits.max_connections)),
        requests: Arc::new(Semaphore::new(limits.max_inflight_requests)),
        stop: CancellationToken::new(),
    };
    let mut tasks = JoinSet::new();
    for listener in listeners {
        listener.socket.set_nonblocking(true)?;
        let socket = tokio::net::TcpListener::from_std(listener.socket)?;
        let tls = listener.secure.then(|| tls.clone()).flatten();
        tasks.spawn(accept(socket, tls, shared.clone()));
    }
    shutdown.await;
    shared.stop.cancel();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Runs one listener, then waits for the connections it accepted to finish.
async fn accept(socket: tokio::net::TcpListener, tls: Option<Arc<Certificates>>, shared: Shared) {
    let mut connections = JoinSet::new();
    let mut backoff = MIN_ACCEPT_BACKOFF;
    loop {
        let accepted = tokio::select! {
            accepted = socket.accept() => accepted,
            () = shared.stop.cancelled() => break,
        };
        let Ok((stream, peer)) = accepted else {
            // Failures such as running out of descriptors clear as connections close.
            tokio::select! {
                () = tokio::time::sleep(backoff) => {}
                () = shared.stop.cancelled() => break,
            }
            backoff = (backoff * 2).min(MAX_ACCEPT_BACKOFF);
            continue;
        };
        backoff = MIN_ACCEPT_BACKOFF;
        // Admit without waiting, so that a full server drops new connections rather than having
        // one listener hold the last permit while idle and starve the other.
        let Ok(permit) = shared.connections.clone().try_acquire_owned() else {
            continue;
        };
        // A handshake uses the certificates current when its connection arrived.
        let material = tls.as_ref().map(|tls| tls.current());
        connections.spawn(connection(stream, peer, permit, material, shared.clone()));
        while connections.try_join_next().is_some() {}
    }
    while connections.join_next().await.is_some() {}
}

/// Completes the handshake, if any, then serves requests. Failures only affect this client.
async fn connection(
    stream: TcpStream,
    peer: SocketAddr,
    permit: OwnedSemaphorePermit,
    tls: Option<Arc<Material>>,
    shared: Shared,
) {
    let Some(material) = tls else {
        let io = Deadlines::new(stream, permit, &shared.limits);
        serve(io, peer, None, shared).await;
        return;
    };
    let acceptor = TlsAcceptor::from(material.config.clone());
    let handshake = tokio::time::timeout(shared.limits.handshake_timeout, acceptor.accept(stream));
    let Ok(Ok(stream)) = handshake.await else {
        return;
    };
    let report = material.describe(stream.get_ref().1);
    let io = Deadlines::new(stream, permit, &shared.limits);
    serve(io, peer, Some(report), shared).await;
}

/// Serves requests on one connection, which finishes those in flight once the server stops.
async fn serve<I>(io: I, peer: SocketAddr, tls: Option<TlsReport>, shared: Shared)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let limits = shared.limits;
    let activity = Activity::new();
    let requests = shared.requests.clone();
    let app = shared.app.clone();
    let tracked = activity.clone();
    let service = tower::service_fn(move |mut request: Request<Incoming>| {
        let extensions = request.extensions_mut();
        extensions.insert(ConnectInfo(peer));
        if let Some(tls) = &tls {
            extensions.insert(tls.clone());
        }
        admit(request, app.clone(), requests.clone(), tracked.clone())
    });
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout);
    // HTTP/2 clients can only open WebSockets if the server advertises extended CONNECT.
    builder
        .http2()
        .enable_connect_protocol()
        .timer(TokioTimer::new())
        .keep_alive_interval(limits.write_idle_timeout)
        .max_concurrent_streams(u32::try_from(limits.max_inflight_requests).unwrap_or(u32::MAX));
    let connection =
        builder.serve_connection_with_upgrades(TokioIo::new(io), TowerToHyperService::new(service));
    tokio::pin!(connection);
    tokio::select! {
        _ = &mut connection => {}
        () = shared.stop.cancelled() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
        () = activity.idle_for(limits.idle_timeout) => {
            // A graceful HTTP/2 close waits for the client, which may have gone silent.
            connection.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(limits.write_idle_timeout, connection).await;
        }
    }
}

/// Runs a request if there's capacity, tying that capacity to its response body.
async fn admit(
    mut request: Request<Incoming>,
    app: Router,
    requests: Arc<Semaphore>,
    activity: Arc<Activity>,
) -> Result<Response, Infallible> {
    let Ok(permit) = requests.try_acquire_owned() else {
        let mut response = Response::new(Body::from("request capacity exhausted\n"));
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        return Ok(response);
    };
    let guard = activity.begin();
    // An upgrade outlives its response, so it has to mark itself as a request in progress.
    request.extensions_mut().insert(activity);
    let response = app.oneshot(request).await?;
    Ok(response.map(|body| Body::new(TrackedBody::new(body, permit, guard))))
}
