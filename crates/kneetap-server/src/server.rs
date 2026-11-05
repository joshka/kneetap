use std::future::Future;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use crate::health::Health;
use crate::host::Host;
use crate::routes::{self, Context};
use crate::{Error, grpc};

const DEFAULT_MAX_BODY_SIZE: usize = 1 << 20;
const DEFAULT_MAX_DATA_SIZE: u64 = 10_000_000_000;
const DEFAULT_MAX_WS_MESSAGE_SIZE: usize = 1 << 20;
const DEFAULT_MAX_WAIT: Duration = Duration::from_mins(5);

/// A bound server that hasn't started handling requests.
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    settings: ServerBuilder,
}

impl Server {
    /// Starts configuring a server.
    #[must_use]
    pub fn builder() -> ServerBuilder {
        ServerBuilder::default()
    }

    /// Returns the bound address, which reveals the port when binding to port 0.
    ///
    /// # Panics
    ///
    /// Panics if the operating system can't report the address of a socket it bound.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound socket has an address")
    }

    /// Handles requests until `shutdown` completes.
    ///
    /// Shutdown stops accepting connections and waits for requests in progress to finish.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket can't be handed to the async runtime.
    pub async fn serve<F>(self, shutdown: F) -> Result<(), Error>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.listener.set_nonblocking(true).map_err(Error::serve)?;
        let listener = tokio::net::TcpListener::from_std(self.listener).map_err(Error::serve)?;
        let (reporter, grpc_health) = tonic_health::server::health_reporter();
        let settings = self.settings;
        let context = Arc::new(Context {
            name: settings.name,
            host: Host::detect(),
            max_body_size: settings.max_body_size,
            max_wait: settings.max_wait,
            enable_env: settings.enable_env,
            max_data_size: settings.max_data_size,
            health: Health::new(reporter).await,
            allow_any_ws_origin: settings.allow_any_ws_origin,
            max_ws_message_size: settings.max_ws_message_size,
        });
        let app = grpc::router(context.clone(), grpc_health)
            .fallback_service(routes::router(context.clone()));
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service)
            .with_graceful_shutdown(async move {
                shutdown.await;
                context.health.stop_watches().await;
            })
            .await
            .map_err(Error::serve)
    }
}

/// Configures a [`Server`].
#[derive(Debug)]
pub struct ServerBuilder {
    name: Option<String>,
    max_body_size: usize,
    max_wait: Duration,
    enable_env: bool,
    max_data_size: u64,
    allow_any_ws_origin: bool,
    max_ws_message_size: usize,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self {
            name: None,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            max_wait: DEFAULT_MAX_WAIT,
            enable_env: false,
            max_data_size: DEFAULT_MAX_DATA_SIZE,
            allow_any_ws_origin: false,
            max_ws_message_size: DEFAULT_MAX_WS_MESSAGE_SIZE,
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

    /// Lets browsers open `/echo` from any site, which is off by default so that a web page can't
    /// reach a server only its visitor can see.
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

    /// Binds the listening socket without serving yet.
    ///
    /// This is synchronous so a process can bind before starting a runtime, then drop privileges.
    ///
    /// # Errors
    ///
    /// Returns an error if the address can't be bound.
    pub fn bind(self, address: impl ToSocketAddrs) -> Result<Server, Error> {
        let listener = TcpListener::bind(address).map_err(Error::bind)?;
        Ok(Server {
            listener,
            settings: self,
        })
    }
}
