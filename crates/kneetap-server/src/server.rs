use std::future::Future;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::health::Health;
use crate::host::Host;
use crate::identity::Identity;
use crate::limits::Limits;
use crate::reload::{self, Certificates, ReloadHandle};
use crate::routes::{self, Context};
use crate::status::Status;
use crate::tls::{Material, TlsConfig};
use crate::transport::{self, Listener};
use crate::{Error, grpc};

const DEFAULT_MAX_BODY_SIZE: usize = 1 << 20;

/// Large enough to test transfers, small enough that one request can't tie up a link for days.
const DEFAULT_MAX_DATA_SIZE: u64 = 10_000_000_000;
const DEFAULT_MAX_WS_MESSAGE_SIZE: usize = 1 << 20;
const DEFAULT_MAX_WAIT: Duration = Duration::from_mins(5);
const DEFAULT_RELOAD_INTERVAL: Duration = Duration::from_secs(30);

/// A bound server that hasn't started handling requests.
#[derive(Debug)]
pub struct Server {
    listeners: Vec<Listener>,
    tls: Option<Material>,
    reload: ReloadHandle,
    settings: ServerBuilder,
}

impl Server {
    /// Starts configuring a server.
    #[must_use]
    pub fn builder() -> ServerBuilder {
        ServerBuilder::default()
    }

    /// Returns the first bound address, which reveals the port when binding to port 0.
    ///
    /// # Panics
    ///
    /// Panics if the operating system can't report the address of a socket it bound.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.listeners[0].local_addr()
    }

    /// Returns the URL of each listener, with plain HTTP first.
    ///
    /// # Panics
    ///
    /// Panics if the operating system can't report the address of a socket it bound.
    #[must_use]
    pub fn urls(&self) -> Vec<String> {
        self.listeners.iter().map(Listener::url).collect()
    }

    /// Returns a handle that asks the server to reread its certificates.
    #[must_use]
    pub fn reload_handle(&self) -> ReloadHandle {
        self.reload.clone()
    }

    /// Handles requests until `shutdown` completes.
    ///
    /// Shutdown reports unhealthy, stops accepting connections, closes `WebSocket` connections and
    /// quiet gRPC streams, and lets requests in progress finish, for up to the [shutdown
    /// timeout](ServerBuilder::shutdown_timeout).
    ///
    /// # Errors
    ///
    /// Returns an error if a socket can't be handed to the async runtime.
    pub async fn serve<F>(self, shutdown: F) -> Result<(), Error>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let stop = CancellationToken::new();
        let websockets = TaskTracker::new();
        let (reporter, grpc_health) = tonic_health::server::health_reporter();
        let settings = self.settings;
        let status = Status::new(settings.degradations);
        let tls = self.tls.zip(settings.tls);
        let certificates = tls.map(|(tls, config)| Certificates::new(tls, config, status.clone()));
        let certificates = certificates.map(Arc::new);
        let health = Health::new(reporter, settings.name.as_deref()).await;
        let context = Arc::new(Context {
            identity: Identity::new(settings.name, Host::detect(), settings.commit),
            max_body_size: settings.max_body_size,
            max_wait: settings.max_wait,
            enable_env: settings.enable_env,
            max_data_size: settings.max_data_size,
            health,
            allow_any_ws_origin: settings.allow_any_ws_origin,
            max_ws_message_size: settings.max_ws_message_size,
            status,
            body_timeout: settings.limits.body_timeout,
            write_idle_timeout: settings.limits.write_idle_timeout,
            idle_timeout: settings.limits.idle_timeout,
            shutdown_timeout: settings.limits.shutdown_timeout,
            websockets: websockets.clone(),
            stop: stop.clone(),
        });
        let draining = context.clone();
        let app =
            grpc::router(context.clone(), grpc_health).fallback_service(routes::router(context));
        let interval = settings.reload_interval;
        let _reloads = certificates
            .clone()
            .map(|tls| reload::spawn(tls, self.reload, interval));
        let trigger = stop.clone();
        tokio::spawn(async move {
            shutdown.await;
            tracing::info!("server.shutdown.started");
            draining.health.drain().await;
            // Watchers see the drain first, because ending a watch delivers its last status.
            draining.health.stop_watches().await;
            trigger.cancel();
        });
        let limits = settings.limits;
        let served = transport::run(self.listeners, app, certificates, limits, stop, websockets);
        served.await.map_err(Error::serve)
    }
}

/// Configures a [`Server`].
#[derive(Debug)]
pub struct ServerBuilder {
    name: Option<String>,
    commit: Option<String>,
    max_body_size: usize,
    max_wait: Duration,
    enable_env: bool,
    max_data_size: u64,
    allow_any_ws_origin: bool,
    max_ws_message_size: usize,
    http: Vec<TcpListener>,
    https: Vec<TcpListener>,
    tls: Option<TlsConfig>,
    reload_interval: Duration,
    limits: Limits,
    degradations: Vec<String>,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self {
            name: None,
            commit: None,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            max_wait: DEFAULT_MAX_WAIT,
            enable_env: false,
            max_data_size: DEFAULT_MAX_DATA_SIZE,
            allow_any_ws_origin: false,
            max_ws_message_size: DEFAULT_MAX_WS_MESSAGE_SIZE,
            http: Vec::new(),
            https: Vec::new(),
            tls: None,
            reload_interval: DEFAULT_RELOAD_INTERVAL,
            limits: Limits::default(),
            degradations: Vec::new(),
        }
    }
}

impl ServerBuilder {
    /// Sets the name included in reports, which distinguishes replicas on one host.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the commit included in reports, which the program that runs the server may know.
    #[must_use]
    pub fn commit(mut self, commit: impl Into<String>) -> Self {
        self.commit = Some(commit.into());
        self
    }

    /// Sets the largest request body a report will read. The default is 1 MiB.
    #[must_use]
    pub const fn max_body_size(mut self, bytes: usize) -> Self {
        self.max_body_size = bytes;
        self
    }

    /// Sets the longest delay a client can request with `?wait=`. The default is 5 minutes.
    #[must_use]
    pub const fn max_wait(mut self, wait: Duration) -> Self {
        self.max_wait = wait;
        self
    }

    /// Allows clients to read the environment with `?env=true`, which is off by default because
    /// environment variables often hold secrets.
    #[must_use]
    pub const fn enable_env(mut self) -> Self {
        self.enable_env = true;
        self
    }

    /// Sets the largest response a client can request from `/data`. The default is 10 GB.
    #[must_use]
    pub const fn max_data_size(mut self, bytes: u64) -> Self {
        self.max_data_size = bytes;
        self
    }

    /// Lets browsers open `/echo` from any site.
    ///
    /// By default a page's origin must match the host and port it asked for. The scheme is ignored,
    /// and DNS rebinding gets past the check, so it isn't access control for a private server.
    #[must_use]
    pub const fn allow_any_ws_origin(mut self) -> Self {
        self.allow_any_ws_origin = true;
        self
    }

    /// Sets the largest WebSocket message accepted. The default is 1 MiB.
    #[must_use]
    pub const fn max_ws_message_size(mut self, bytes: usize) -> Self {
        self.max_ws_message_size = bytes;
        self
    }

    /// Sets the certificate and client authentication for HTTPS listeners.
    #[must_use]
    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Sets how often certificate files are checked for changes. The default is 30 seconds, and
    /// zero turns checking off.
    #[must_use]
    pub const fn reload_interval(mut self, interval: Duration) -> Self {
        self.reload_interval = interval;
        self
    }

    /// Sets how many connections can be open at once, across all listeners. The default is 1024.
    ///
    /// Further connections are closed as soon as they're accepted.
    #[must_use]
    pub const fn max_connections(mut self, connections: usize) -> Self {
        self.limits.max_connections = connections;
        self
    }

    /// Sets how many requests can run at once. The default is 1024.
    ///
    /// Further requests get a 503 until a response finishes or its client disconnects.
    #[must_use]
    pub const fn max_inflight_requests(mut self, requests: usize) -> Self {
        self.limits.max_inflight_requests = requests;
        self
    }

    /// Sets the time allowed for a TLS handshake. The default is 10 seconds.
    #[must_use]
    pub const fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.limits.handshake_timeout = timeout;
        self
    }

    /// Sets the time a connection has to send its first bytes and complete its request headers.
    /// The default is 10 seconds.
    #[must_use]
    pub const fn header_timeout(mut self, timeout: Duration) -> Self {
        self.limits.header_timeout = timeout;
        self
    }

    /// Sets the time allowed to read a request body. The default is 30 seconds.
    #[must_use]
    pub const fn body_timeout(mut self, timeout: Duration) -> Self {
        self.limits.body_timeout = timeout;
        self
    }

    /// Sets how long a write may wait for a client to read. The default is 30 seconds.
    ///
    /// An HTTP/2 connection closes when a response goes this long without the client's flow-control
    /// window letting its next chunk out. `/data` sends 64 KiB chunks, so the slowest download that
    /// survives the default is about 2 KiB per second.
    #[must_use]
    pub const fn write_idle_timeout(mut self, timeout: Duration) -> Self {
        self.limits.write_idle_timeout = timeout;
        self
    }

    /// Sets how long a connection may stay open with no requests, and how long a `WebSocket` or
    /// gRPC stream may stay open with nothing to deliver. The default is 60 seconds.
    ///
    /// A `WebSocket` counts client messages, not pings. A gRPC stream counts messages it sends, and
    /// ends as `UNAVAILABLE` so that a health watcher knows to call again.
    #[must_use]
    pub const fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.limits.idle_timeout = timeout;
        self
    }

    /// Sets how long shutdown waits for work in progress. The default is 10 seconds.
    #[must_use]
    pub const fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.limits.shutdown_timeout = timeout;
        self
    }

    /// Records what the caller worked around at startup, so that reports can show it.
    #[must_use]
    pub fn degradations(mut self, degradations: Vec<String>) -> Self {
        self.degradations = degradations;
        self
    }

    /// Adds a bound socket that serves plain HTTP.
    #[must_use]
    pub fn http_listener(mut self, listener: TcpListener) -> Self {
        self.http.push(listener);
        self
    }

    /// Adds a bound socket that serves HTTPS, which requires [`tls`](Self::tls).
    #[must_use]
    pub fn https_listener(mut self, listener: TcpListener) -> Self {
        self.https.push(listener);
        self
    }

    /// Binds a plain HTTP socket without serving yet.
    ///
    /// This is synchronous so a process can bind before starting a runtime, then drop privileges.
    ///
    /// # Errors
    ///
    /// Returns an error if the address can't be bound.
    pub fn bind(self, address: impl ToSocketAddrs) -> Result<Server, Error> {
        let listener = TcpListener::bind(address).map_err(Error::bind)?;
        self.http_listener(listener).build()
    }

    /// Validates the configuration and prepares its listeners without serving yet.
    ///
    /// Certificates are read here, so that a process can give up the right to read them afterward.
    ///
    /// # Errors
    ///
    /// Returns an error if there are no listeners, or HTTPS can't be set up.
    pub fn build(mut self) -> Result<Server, Error> {
        let http = std::mem::take(&mut self.http);
        let https = std::mem::take(&mut self.https);
        if http.is_empty() && https.is_empty() {
            return Err(Error::no_listeners());
        }
        if !https.is_empty() && self.tls.is_none() {
            return Err(Error::missing_tls());
        }
        let tls = match &self.tls {
            Some(tls) if !https.is_empty() => Some(Material::load(tls)?),
            _ => None,
        };
        let listeners = http
            .into_iter()
            .map(Listener::http)
            .chain(https.into_iter().map(Listener::https))
            .collect();
        Ok(Server {
            listeners,
            tls,
            reload: ReloadHandle::default(),
            settings: self,
        })
    }
}
