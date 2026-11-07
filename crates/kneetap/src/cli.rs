use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::builder::BoolishValueParser;
use clap::{Args, Parser, Subcommand};
use kneetap_client::{Client, ClientBuilder};
#[cfg(unix)]
use kneetap_server::ReloadHandle;
use kneetap_server::{Server, TlsConfig};

use crate::privileges;
use crate::startup::{
    self, DEFAULT_HTTP_PORT, DEFAULT_TLS_PORT, FALLBACK_HTTP_PORT, FALLBACK_TLS_PORT, PortChoice,
    Strictness,
};
use crate::telemetry::{self, LogFormat, Verbosity};

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Log debug messages; `KNEETAP_LOG` takes priority
    #[arg(long, global = true)]
    verbose: bool,

    /// How to write log messages
    #[arg(long, global = true, value_enum, default_value_t)]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve reports until interrupted
    Serve(Serve),

    /// Exit successfully only if a server reports that it's healthy
    Healthcheck(Healthcheck),

    /// Print a server's report
    Report(ReportArgs),
}

#[derive(Debug, Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent command-line flag"
)]
struct Serve {
    /// Address shared by all listeners [default: all IPv6 and IPv4 addresses]
    #[arg(long, env = "KNEETAP_BIND")]
    bind: Option<IpAddr>,

    /// HTTP port [default: 80, or 8080 if 80 can't be bound]
    #[arg(long, env = "KNEETAP_PORT")]
    port: Option<u16>,

    /// Name included in reports
    #[arg(long, env = "KNEETAP_NAME")]
    name: Option<String>,

    /// Allow `?env=true` to return the server's environment variables
    #[arg(long, env = "KNEETAP_ENABLE_ENV", value_parser = BoolishValueParser::new())]
    enable_env: bool,

    /// Permit browsers to connect to `/echo` from any origin
    #[arg(long, env = "KNEETAP_ALLOW_ANY_WS_ORIGIN", value_parser = BoolishValueParser::new())]
    allow_any_ws_origin: bool,

    /// HTTPS port [default: 443, or 8443 if 443 can't be bound]
    #[arg(long, env = "KNEETAP_TLS_PORT")]
    tls_port: Option<u16>,

    /// Don't serve plain HTTP
    #[arg(long, env = "KNEETAP_NO_HTTP", value_parser = BoolishValueParser::new())]
    no_http: bool,

    /// Don't serve HTTPS, whatever the TLS options say
    #[arg(long, env = "KNEETAP_NO_TLS", value_parser = BoolishValueParser::new())]
    no_tls: bool,

    /// Exit at startup on any failure instead of continuing with what works
    #[arg(long, env = "KNEETAP_STRICT", value_parser = BoolishValueParser::new())]
    strict: bool,

    /// TLS certificate chain in PEM format, instead of a generated one
    #[arg(long, env = "KNEETAP_CERT", requires = "key")]
    cert: Option<PathBuf>,

    /// TLS private key in PEM format
    #[arg(long, env = "KNEETAP_KEY", requires = "cert")]
    key: Option<PathBuf>,

    /// Authorities in PEM format; HTTPS clients must present a certificate they signed
    #[arg(long, env = "KNEETAP_CACERT")]
    cacert: Option<PathBuf>,

    /// How often to check TLS files for changes; `0s` turns this off
    #[arg(long, env = "KNEETAP_TLS_RELOAD_INTERVAL", default_value = "30s", value_parser = humantime::parse_duration)]
    tls_reload_interval: Duration,

    /// Most connections open at once, across all listeners
    #[arg(long, env = "KNEETAP_MAX_CONNECTIONS", default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..))]
    max_connections: u32,

    /// Most requests handled at once, across all connections
    #[arg(long, env = "KNEETAP_MAX_INFLIGHT_REQUESTS", default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..))]
    max_inflight_requests: u32,

    /// Time allowed for a TLS handshake
    #[arg(long, env = "KNEETAP_HANDSHAKE_TIMEOUT", default_value = "10s", value_parser = positive_duration)]
    handshake_timeout: Duration,

    /// Time allowed for a connection to send its first bytes and complete its headers
    #[arg(long, env = "KNEETAP_HEADER_TIMEOUT", default_value = "10s", value_parser = positive_duration)]
    header_timeout: Duration,

    /// Time allowed to read a request body, or between messages on a gRPC stream
    #[arg(long, env = "KNEETAP_BODY_TIMEOUT", default_value = "30s", value_parser = positive_duration)]
    body_timeout: Duration,

    /// Time a blocked write may wait for the client to read
    #[arg(long, env = "KNEETAP_WRITE_IDLE_TIMEOUT", default_value = "30s", value_parser = positive_duration)]
    write_idle_timeout: Duration,

    /// Time a connection may stay open with no requests
    #[arg(long, env = "KNEETAP_IDLE_TIMEOUT", default_value = "60s", value_parser = positive_duration)]
    idle_timeout: Duration,

    /// Time allowed for work in progress to finish after a shutdown signal
    #[arg(long, env = "KNEETAP_SHUTDOWN_TIMEOUT", default_value = "10s", value_parser = humantime::parse_duration)]
    shutdown_timeout: Duration,

    /// User ID to run as after binding, when started as root [default: 65532]
    #[arg(long, env = "PUID")]
    puid: Option<u32>,

    /// Group ID to run as after binding, when started as root [default: 65532]
    #[arg(long, env = "PGID")]
    pgid: Option<u32>,
}

/// Parses a duration, refusing zero because it would close every connection at once.
fn positive_duration(value: &str) -> Result<Duration, String> {
    match humantime::parse_duration(value) {
        Ok(duration) if !duration.is_zero() => Ok(duration),
        Ok(_) => Err("must be greater than zero".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// How a client command connects to a server.
#[derive(Debug, Args)]
struct ClientTls {
    /// Accept any server certificate, such as a generated one
    #[arg(long, conflicts_with = "cacert")]
    insecure: bool,

    /// Trust only the authorities in this PEM file
    #[arg(long)]
    cacert: Option<PathBuf>,

    /// Client certificate chain in PEM format
    #[arg(long, requires = "key")]
    cert: Option<PathBuf>,

    /// Client private key in PEM format
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,
}

impl ClientTls {
    /// Adds these options to a client, which reads the files when it's built.
    fn apply(&self, mut builder: ClientBuilder) -> ClientBuilder {
        if self.insecure {
            builder = builder.danger_accept_invalid_certs();
        }
        if let Some(path) = &self.cacert {
            builder = builder.root_certificates_file(path);
        }
        if let (Some(certificate), Some(key)) = (&self.cert, &self.key) {
            builder = builder.identity_files(certificate, key);
        }
        builder
    }
}

#[derive(Debug, Args)]
struct Healthcheck {
    /// Health endpoint to check, instead of finding a local server
    #[arg(long)]
    url: Option<String>,

    /// Address of the local server, as given to `serve`
    #[arg(long, env = "KNEETAP_BIND")]
    bind: Option<IpAddr>,

    /// HTTP port of the local server, as given to `serve`
    #[arg(long, env = "KNEETAP_PORT")]
    port: Option<u16>,

    /// HTTPS port of the local server, as given to `serve`
    #[arg(long, env = "KNEETAP_TLS_PORT")]
    tls_port: Option<u16>,

    /// The local server doesn't serve plain HTTP
    #[arg(long, env = "KNEETAP_NO_HTTP", value_parser = BoolishValueParser::new())]
    no_http: bool,

    /// The local server doesn't serve HTTPS
    #[arg(long, env = "KNEETAP_NO_TLS", value_parser = BoolishValueParser::new())]
    no_tls: bool,

    /// Name of the local server, as given to `serve`; other servers are ignored
    #[arg(long, env = "KNEETAP_NAME")]
    name: Option<String>,

    /// Client authorities of the local server, as given to `serve`; its refusal to talk without a
    /// client certificate then counts as success, though it can't show the health status
    #[arg(long, env = "KNEETAP_CACERT")]
    server_cacert: Option<PathBuf>,

    /// Longest the whole check may take
    #[arg(long, default_value = "5s", value_parser = humantime::parse_duration)]
    timeout: Duration,

    #[command(flatten)]
    tls: ClientTls,
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Base URL of the server
    url: String,

    /// Print JSON instead of text
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    tls: ClientTls,
}

/// Runs the command line of either binary.
///
/// # Errors
///
/// Returns an error for a failed command, which the binaries turn into a non-zero exit.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let verbosity = if cli.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    };
    telemetry::init(cli.log_format, verbosity);
    match cli.command {
        Command::Serve(serve) => run_server(&serve),
        Command::Healthcheck(check) => block_on(run_healthcheck(check)),
        Command::Report(report) => block_on(run_report(report)),
    }
}

/// Runs a client command, which needs no more than one thread.
fn block_on(command: impl Future<Output = Result<()>>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(command);
    // Dropping the runtime would wait for a slow DNS lookup, past the command's own timeout.
    runtime.shutdown_background();
    result
}

/// Probes a given URL, or finds the local server. A deadline covers the whole check, because a
/// probe that tries several ports would otherwise take several timeouts.
async fn run_healthcheck(check: Healthcheck) -> Result<()> {
    let builder = check.tls.apply(Client::builder().timeout(check.timeout));
    // A local server's certificate is usually generated, and this machine needs no proof of who it
    // is. An explicit URL is always verified.
    let client = if check.url.is_none() {
        builder.danger_accept_invalid_certs()
    } else {
        builder
    }
    .build()?;
    let probe = async {
        match &check.url {
            Some(url) => client.check_health(url).await,
            None => discover(&client, &Endpoint::discover(&check)).await,
        }
    };
    tokio::time::timeout(check.timeout, probe)
        .await
        .context("health check timed out")??;
    Ok(())
}

/// Checks a local server. One that requires client certificates may refuse us, as an image's
/// default health check has none, and that's the only answer such a server can give.
async fn discover(client: &Client, endpoint: &Endpoint) -> Result<(), kneetap_client::Error> {
    let name = endpoint.name.as_deref();
    let error = match client.discover_health(&endpoint.urls(), name).await {
        Err(error) if endpoint.requires_client_auth && error.unhealthy_status().is_none() => error,
        result => return result,
    };
    if client
        .requires_client_certificate(&endpoint.tls_addresses())
        .await
    {
        tracing::warn!("healthcheck.status.unverified");
        return Ok(());
    }
    Err(error)
}

/// Where a local server should be listening.
#[derive(Debug)]
struct Endpoint {
    bind: Option<IpAddr>,
    port: Option<u16>,
    tls_port: Option<u16>,
    no_http: bool,
    no_tls: bool,
    name: Option<String>,
    requires_client_auth: bool,
}

impl Endpoint {
    /// Uses the command's own settings, or else those of the server this container is running.
    fn discover(check: &Healthcheck) -> Self {
        let endpoint = Self {
            bind: check.bind,
            port: check.port,
            tls_port: check.tls_port,
            no_http: check.no_http,
            no_tls: check.no_tls,
            name: check.name.clone(),
            requires_client_auth: check.server_cacert.is_some(),
        };
        if !endpoint.is_unset() {
            return endpoint;
        }
        let Some(container) = container_endpoint() else {
            return endpoint;
        };
        Self {
            name: endpoint.name.or(container.name),
            requires_client_auth: endpoint.requires_client_auth || container.requires_client_auth,
            ..container
        }
    }

    /// Returns whether the command was told nothing, so it should look for a running server
    /// instead.
    const fn is_unset(&self) -> bool {
        self.bind.is_none()
            && self.port.is_none()
            && self.tls_port.is_none()
            && !self.no_http
            && !self.no_tls
    }

    /// Lists the URLs to try, HTTP first, since either listener may be the one that started.
    ///
    /// Only an implicit port can have moved to its fallback.
    fn urls(&self) -> Vec<String> {
        let candidates = |scheme, port: Option<u16>, default, fallback| {
            let ports = port.map_or(vec![default, fallback], |port| vec![port]);
            ports
                .into_iter()
                .map(move |port| format!("{scheme}://{}/health", probe_target(self.bind, port)))
        };
        let http = candidates("http", self.port, DEFAULT_HTTP_PORT, FALLBACK_HTTP_PORT);
        let https = candidates("https", self.tls_port, DEFAULT_TLS_PORT, FALLBACK_TLS_PORT);
        match (self.no_http, self.no_tls) {
            (true, _) => https.collect(),
            (_, true) => http.collect(),
            _ => http.chain(https).collect(),
        }
    }

    /// Lists where HTTPS may be listening, for checks that can't go through HTTP.
    fn tls_addresses(&self) -> Vec<SocketAddr> {
        if self.no_tls {
            return Vec::new();
        }
        let ports = self
            .tls_port
            .map_or(vec![DEFAULT_TLS_PORT, FALLBACK_TLS_PORT], |port| vec![port]);
        ports
            .into_iter()
            .map(|port| probe_target(self.bind, port))
            .collect()
    }
}

/// Reads PID 1's arguments, because `docker exec` and `HEALTHCHECK` don't inherit them.
#[cfg(target_os = "linux")]
fn container_endpoint() -> Option<Endpoint> {
    let bytes = std::fs::read("/proc/1/cmdline").ok()?;
    let arguments = bytes
        .split(|&byte| byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument).into_owned());
    let Command::Serve(serve) = Cli::try_parse_from(arguments).ok()?.command else {
        return None;
    };
    Some(Endpoint {
        bind: serve.bind,
        port: serve.port,
        tls_port: serve.tls_port,
        no_http: serve.no_http,
        no_tls: serve.no_tls,
        name: serve.name,
        requires_client_auth: serve.cacert.is_some(),
    })
}

#[cfg(not(target_os = "linux"))]
const fn container_endpoint() -> Option<Endpoint> {
    None
}

/// Finds where to reach a local server, since a wildcard address can't be connected to.
fn probe_target(bind: Option<IpAddr>, port: u16) -> SocketAddr {
    let address = match bind {
        Some(IpAddr::V6(ip)) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        Some(ip) if !ip.is_unspecified() => ip,
        _ => Ipv4Addr::LOCALHOST.into(),
    };
    SocketAddr::new(address, port)
}

/// Prints a report. JSON escapes control characters itself, so only text needs [`printable`].
async fn run_report(args: ReportArgs) -> Result<()> {
    let client = args.tls.apply(Client::builder()).build()?;
    let url = format!("{}/api", args.url.trim_end_matches('/'));
    let report = client.report(&url).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", printable(&report.to_string()));
    }
    Ok(())
}

/// Binds before starting the runtime, so a failure to bind is reported without starting threads.
fn run_server(serve: &Serve) -> Result<()> {
    let mut builder = Server::builder()
        .reload_interval(serve.tls_reload_interval)
        .max_connections(serve.max_connections as usize)
        .max_inflight_requests(serve.max_inflight_requests as usize)
        .handshake_timeout(serve.handshake_timeout)
        .header_timeout(serve.header_timeout)
        .body_timeout(serve.body_timeout)
        .write_idle_timeout(serve.write_idle_timeout)
        .idle_timeout(serve.idle_timeout)
        .shutdown_timeout(serve.shutdown_timeout);
    if serve.enable_env {
        builder = builder.enable_env();
    }
    if serve.allow_any_ws_origin {
        builder = builder.allow_any_ws_origin();
    }
    if let Some(name) = &serve.name {
        builder = builder.name(name);
    }
    let strictness = if serve.strict {
        Strictness::Strict
    } else {
        Strictness::Lenient
    };
    let mut degradations = Vec::new();
    let http = if serve.no_http {
        None
    } else {
        let choice = PortChoice {
            explicit: serve.port,
            default: DEFAULT_HTTP_PORT,
            fallback: FALLBACK_HTTP_PORT,
            degradation: "http_bind_failed",
        };
        startup::open(serve.bind, choice, strictness, &mut degradations)?
    };
    let https = if serve.no_tls {
        None
    } else {
        let choice = PortChoice {
            explicit: serve.tls_port,
            default: DEFAULT_TLS_PORT,
            fallback: FALLBACK_TLS_PORT,
            degradation: "https_bind_failed",
        };
        startup::open(serve.bind, choice, strictness, &mut degradations)?
    };
    let tls = match https {
        Some(_) => usable_tls(serve, &mut degradations)?,
        None => None,
    };
    if serve.cacert.is_some() && http.is_some() {
        tracing::warn!("listener.http.unauthenticated");
    }
    if let Some(listener) = http {
        builder = builder.http_listener(listener);
    }
    if let (Some(listener), Some(tls)) = (https, tls) {
        builder = builder.https_listener(listener).tls(tls);
    }
    builder = builder.degradations(degradations);
    let server = builder.build()?;
    privileges::drop_privileges(serve.puid, serve.pgid)?;
    let _logs = telemetry::write_in_background();
    // Stdout is the interface scripts use to find the port, so these aren't log events.
    for url in server.urls() {
        println!("listening on {url}");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // Claiming `SIGHUP` replaces its default of ending the process, so only do that if it's useful.
    #[cfg(unix)]
    if server.urls().iter().any(|url| url.starts_with("https:")) {
        runtime.spawn(forward_hangups(server.reload_handle()));
    }
    runtime.block_on(server.serve(shutdown_signal()))?;
    Ok(())
}

/// Escapes control characters, because a report's strings come from the server and could otherwise
/// drive the terminal.
fn printable(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && !matches!(character, '\n' | '\t') {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

/// Completes when the process is asked to stop, by an interrupt or a container runtime's `SIGTERM`.
///
/// A signal that can't be installed is ignored, and with neither the server runs until it's killed.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let Ok(mut terminate) = signal(SignalKind::terminate()) else {
            return interrupted().await;
        };
        tokio::select! {
            () = interrupted() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    interrupted().await;
}

/// Completes on an interrupt, or never if the interrupt handler can't be installed.
async fn interrupted() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Treats `SIGHUP` as a request to reread certificates, as daemons conventionally do.
#[cfg(unix)]
async fn forward_hangups(reload: ReloadHandle) {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut hangups) = signal(SignalKind::hangup()) else {
        return;
    };
    while hangups.recv().await.is_some() {
        reload.reload();
    }
}

/// Chooses the TLS configuration to serve, or `None` to go without HTTPS.
///
/// An unusable certificate falls back to a generated one, which still encrypts. A broken client
/// authority never does: HTTPS stops rather than serving clients it was told to authenticate.
fn usable_tls(serve: &Serve, degradations: &mut Vec<String>) -> Result<Option<TlsConfig>> {
    let tls = tls_config(serve);
    let Err(error) = tls.validate() else {
        return Ok(Some(tls));
    };
    if serve.strict {
        return Err(error.into());
    }
    tracing::warn!(%error, "tls.setup.failed");
    if error.is_tls_identity() {
        let generated = tls.with_generated_identity();
        if generated.validate().is_ok() {
            degradations.push("server_identity_fallback".to_owned());
            return Ok(Some(generated));
        }
    }
    degradations.push("tls_setup_failed".to_owned());
    Ok(None)
}

/// Translates the flags. `--cert` requires `--key`, so a partial pair can't reach this point.
fn tls_config(serve: &Serve) -> TlsConfig {
    let tls = match (&serve.cert, &serve.key) {
        (Some(certificate), Some(key)) => TlsConfig::files(certificate, key),
        _ => TlsConfig::generated(),
    };
    match &serve.cacert {
        Some(path) => tls.client_ca(path),
        None => tls,
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(false, false, &[
        "http://127.0.0.1:80/health",
        "http://127.0.0.1:8080/health",
        "https://127.0.0.1:443/health",
        "https://127.0.0.1:8443/health",
    ])]
    #[case(true, false, &["https://127.0.0.1:443/health", "https://127.0.0.1:8443/health"])]
    #[case(false, true, &["http://127.0.0.1:80/health", "http://127.0.0.1:8080/health"])]
    fn healthcheck_tries_each_listener_that_may_have_started(
        #[case] no_http: bool,
        #[case] no_tls: bool,
        #[case] urls: &[&str],
    ) {
        let endpoint = Endpoint {
            bind: None,
            port: None,
            tls_port: None,
            no_http,
            no_tls,
            name: None,
            requires_client_auth: false,
        };

        assert_eq!(endpoint.urls(), urls);
    }

    #[test]
    fn terminal_escapes_in_reports_are_shown_rather_than_run() {
        assert_eq!(printable("a\u{1b}[2Jb\tc\n"), "a\\u{1b}[2Jb\tc\n");
    }

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn insecure_refuses_a_cacert_it_would_ignore() {
        let arguments = ["kneetap", "report", "--insecure", "--cacert", "ca.pem"];

        let error = Cli::try_parse_from(arguments).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[rstest]
    #[case(None, "127.0.0.1:80")]
    #[case(Some("0.0.0.0"), "127.0.0.1:80")]
    #[case(Some("::"), "[::1]:80")]
    #[case(Some("::1"), "[::1]:80")]
    #[case(Some("10.0.0.5"), "10.0.0.5:80")]
    fn probes_reach_the_bind_address_or_loopback(#[case] bind: Option<&str>, #[case] target: &str) {
        let bind = bind.map(|bind| bind.parse().unwrap());

        assert_eq!(probe_target(bind, 80).to_string(), target);
    }
}
