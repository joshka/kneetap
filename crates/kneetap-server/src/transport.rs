use std::convert::Infallible;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{StatusCode, Version};
use axum::response::Response;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use kneetap_api::{ConnectionReport, TlsReport};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use crate::identity::{self, Arrival};
use crate::limits::{Activity, Deadlines, Limits, Throttle, TrackedBody};
use crate::reload::Certificates;
use crate::tls::Material;

/// Shortest wait after a failed accept, so running out of file descriptors can't spin the loop.
const MIN_ACCEPT_BACKOFF: Duration = Duration::from_millis(10);

/// Longest wait after repeated failed accepts, so recovery stays prompt.
const MAX_ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// How often refusals are logged while a server is over capacity.
const LOG_INTERVAL: Duration = Duration::from_secs(1);

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

    /// Kept apart so a flood of one kind can't silence the log of the other.
    connection_refusals: Arc<Throttle>,
    request_refusals: Arc<Throttle>,
}

/// Accepts connections on every listener until `stop` is cancelled, then drains them.
///
/// All connections, upgraded or not, share one deadline. When it passes, dropping the tasks ends
/// the remaining connections. `WebSocket` tasks aren't owned here, and have already been told to
/// stop, so one stuck writing ends at its own close deadline.
///
/// Each connection is served directly so that handlers can see facts only it knows, such as the
/// client certificate from the handshake, and so limits can own each connection's lifetime.
pub async fn run(
    listeners: Vec<Listener>,
    app: Router,
    tls: Option<Arc<Certificates>>,
    limits: Limits,
    stop: CancellationToken,
    websockets: TaskTracker,
) -> io::Result<()> {
    let shared = Shared {
        app,
        limits,
        connections: Arc::new(Semaphore::new(limits.max_connections)),
        requests: Arc::new(Semaphore::new(limits.max_inflight_requests)),
        stop: stop.clone(),
        connection_refusals: Arc::default(),
        request_refusals: Arc::default(),
    };
    let mut tasks = JoinSet::new();
    for listener in listeners {
        listener.socket.set_nonblocking(true)?;
        let socket = tokio::net::TcpListener::from_std(listener.socket)?;
        let tls = listener.secure.then(|| tls.clone()).flatten();
        tasks.spawn(accept(socket, tls, shared.clone()));
    }
    stop.cancelled().await;
    websockets.close();
    let drain = async {
        while tasks.join_next().await.is_some() {}
        websockets.wait().await;
    };
    let _ = tokio::time::timeout(limits.shutdown_timeout, drain).await;
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
        let accepted = accepted.inspect_err(|error| {
            tracing::warn!(%error, retry_ms = backoff.as_millis(), "connection.accept.failed");
        });
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
            if shared.connection_refusals.allow(LOG_INTERVAL) {
                tracing::warn!(%peer, "connection.admission.refused");
            }
            continue;
        };
        // Refusing one connection is better than reusing IDs, which would merge connections.
        let opened = Opened::now().inspect_err(|error| {
            tracing::warn!(%peer, %error, "connection.id.failed");
        });
        let Ok(opened) = opened else {
            continue;
        };
        // A handshake uses the certificates current when its connection arrived.
        let material = tls.as_ref().map(|tls| tls.current());
        let task = connection(stream, peer, opened, permit, material, shared.clone());
        connections.spawn(task);
        while connections.try_join_next().is_some() {}
    }
    // Closing now refuses new clients, rather than leaving them queued until the process exits.
    drop(socket);
    while connections.join_next().await.is_some() {}
}

/// Completes the handshake, if any, then serves requests. Failures only affect this client.
///
/// A handshake is abandoned when the server stops, because it has no request to finish.
async fn connection(
    stream: TcpStream,
    peer: SocketAddr,
    opened: Opened,
    permit: OwnedSemaphorePermit,
    tls: Option<Arc<Material>>,
    shared: Shared,
) {
    let Some(material) = tls else {
        let io = Deadlines::new(stream, permit, &shared.limits);
        serve(io, peer, opened, None, shared).await;
        return;
    };
    let acceptor = TlsAcceptor::from(material.config.clone());
    let handshake = tokio::time::timeout(shared.limits.handshake_timeout, acceptor.accept(stream));
    let handshake = tokio::select! {
        handshake = handshake => handshake,
        () = shared.stop.cancelled() => return,
    };
    let Ok(outcome) = handshake else {
        tracing::debug!(%peer, "connection.handshake.timed_out");
        return;
    };
    let outcome = outcome.inspect_err(|error| {
        tracing::debug!(%peer, %error, "connection.handshake.failed");
    });
    let Ok(stream) = outcome else {
        return;
    };
    let report = material.describe(stream.get_ref().1);
    let io = Deadlines::new(stream, permit, &shared.limits);
    serve(io, peer, opened, Some(report), shared).await;
}

/// Serves requests on one connection, which finishes those in flight once the server stops.
///
/// A response stalled by HTTP/2 flow control closes the whole connection, as a blocked socket write
/// does, because hyper offers no way to reset one stream from outside.
async fn serve<I>(io: I, peer: SocketAddr, opened: Opened, tls: Option<TlsReport>, shared: Shared)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let limits = shared.limits;
    let activity = Activity::new();
    let requests = shared.requests.clone();
    let refusals = shared.request_refusals.clone();
    let app = shared.app.clone();
    let tracked = activity.clone();
    let service = tower::service_fn(move |mut request: Request<Incoming>| {
        let extensions = request.extensions_mut();
        extensions.insert(ConnectInfo(peer));
        extensions.insert(opened.next_request());
        if let Some(tls) = &tls {
            extensions.insert(tls.clone());
        }
        admit(
            request,
            app.clone(),
            requests.clone(),
            refusals.clone(),
            tracked.clone(),
        )
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
        .keep_alive_interval(limits.write_idle_timeout);
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
        () = activity.stalled_for(limits.write_idle_timeout) => {
            tracing::debug!(%peer, "connection.response.stalled");
        }
    }
}

/// An accepted connection's identity, and a count of the requests it has started.
#[derive(Clone)]
struct Opened {
    id: String,
    at: String,
    requests: Arc<AtomicU64>,
}

impl Opened {
    /// Records a connection accepted now, with a random ID.
    ///
    /// Random IDs stay distinct across replicas and restarts, which a counter's wouldn't.
    fn now() -> Result<Self, getrandom::Error> {
        Ok(Self {
            id: identity::random_id()?,
            at: identity::timestamp(SystemTime::now()),
            requests: Arc::default(),
        })
    }

    /// Describes the connection for a request that has just started on it.
    ///
    /// HTTP/2 requests on one connection run at once, so the count is shared and atomic.
    fn next_request(&self) -> ConnectionReport {
        ConnectionReport {
            id: self.id.clone(),
            request_number: self.requests.fetch_add(1, Ordering::Relaxed) + 1,
            opened_at: self.at.clone(),
        }
    }
}

/// Runs a request if there's capacity, tying that capacity to its response body.
async fn admit(
    mut request: Request<Incoming>,
    app: Router,
    requests: Arc<Semaphore>,
    refusals: Arc<Throttle>,
    activity: Arc<Activity>,
) -> Result<Response, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let Ok(permit) = requests.try_acquire_owned() else {
        if refusals.allow(LOG_INTERVAL) {
            tracing::warn!(%method, %path, "request.admission.refused");
        }
        return Ok(unavailable("request capacity exhausted\n"));
    };
    let arrival = Arrival::now().inspect_err(|error| {
        tracing::warn!(%method, %path, %error, "request.id.failed");
    });
    // Refusing one request is better than reusing an ID, which would merge two reports.
    let Ok(arrival) = arrival else {
        return Ok(unavailable("request IDs are unavailable\n"));
    };
    request.extensions_mut().insert(arrival);
    let guard = activity.begin();
    let version = request.version();
    // An upgrade outlives its response, so it has to mark itself as a request in progress.
    request.extensions_mut().insert(activity.clone());
    let response = app.oneshot(request).await?;
    // Waiting starts here, because a slow handler isn't the client's doing. HTTP/1 has no flow
    // control of its own, so the socket's write deadline covers it.
    let wait = (version == Version::HTTP_2).then(|| activity.track_response());
    tracing::debug!(%method, %path, status = response.status().as_u16(), "request.handled");
    Ok(response.map(|body| Body::new(TrackedBody::new(body, permit, guard, wait))))
}

/// Refuses a request the server can't take on now.
fn unavailable(message: &'static str) -> Response {
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response
}
