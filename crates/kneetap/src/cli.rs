use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use kneetap_client::{Client, ClientBuilder};
#[cfg(unix)]
use kneetap_server::ReloadHandle;
use kneetap_server::{Server, TlsConfig};

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
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
struct Serve {
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,

    /// Port to listen on
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Name included in reports
    #[arg(long)]
    name: Option<String>,

    /// Allow `?env=true` to return the server's environment variables
    #[arg(long)]
    enable_env: bool,

    /// Permit browsers to connect to `/echo` from any origin
    #[arg(long)]
    allow_any_ws_origin: bool,

    /// Also serve HTTPS on this port
    #[arg(long)]
    tls_port: Option<u16>,

    /// Don't serve plain HTTP
    #[arg(long, requires = "tls_port")]
    no_http: bool,

    /// TLS certificate chain in PEM format, instead of a generated one
    #[arg(long, requires_all = ["key", "tls_port"])]
    cert: Option<PathBuf>,

    /// TLS private key in PEM format
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,

    /// Authorities in PEM format; HTTPS clients must present a certificate they signed
    #[arg(long, requires = "tls_port")]
    cacert: Option<PathBuf>,

    /// How often to check TLS files for changes; `0s` turns this off
    #[arg(long, requires = "tls_port", default_value = "30s")]
    #[arg(value_parser = humantime::parse_duration)]
    tls_reload_interval: Duration,

    /// Most connections open at once, across all listeners
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..))]
    max_connections: u32,

    /// Most requests handled at once, across all connections
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..))]
    max_inflight_requests: u32,

    /// Time allowed for a TLS handshake
    #[arg(long, default_value = "10s", value_parser = positive_duration)]
    handshake_timeout: Duration,

    /// Time allowed for a connection to send its first bytes and complete its headers
    #[arg(long, default_value = "10s", value_parser = positive_duration)]
    header_timeout: Duration,

    /// Time allowed to read a request body, or between messages on a gRPC stream
    #[arg(long, default_value = "30s", value_parser = positive_duration)]
    body_timeout: Duration,

    /// Time a blocked write may wait for the client to read
    #[arg(long, default_value = "30s", value_parser = positive_duration)]
    write_idle_timeout: Duration,

    /// Time a connection may stay open with no requests
    #[arg(long, default_value = "60s", value_parser = positive_duration)]
    idle_timeout: Duration,
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
    #[arg(long)]
    bind: Option<IpAddr>,

    /// Port of the local server
    #[arg(long, default_value_t = 8080)]
    port: u16,

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
    match Cli::parse().command {
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

async fn run_healthcheck(check: Healthcheck) -> Result<()> {
    let client = check
        .tls
        .apply(Client::builder().timeout(check.timeout))
        .build()?;
    let probe = async {
        if let Some(url) = &check.url {
            client.check_health(url).await
        } else {
            let url = format!("http://{}/health", probe_target(check.bind, check.port));
            client.discover_health(&[url]).await
        }
    };
    tokio::time::timeout(check.timeout, probe)
        .await
        .context("health check timed out")??;
    Ok(())
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
        .idle_timeout(serve.idle_timeout);
    if serve.enable_env {
        builder = builder.enable_env();
    }
    if serve.allow_any_ws_origin {
        builder = builder.allow_any_ws_origin();
    }
    if let Some(name) = &serve.name {
        builder = builder.name(name);
    }
    if !serve.no_http {
        builder = builder.http_listener(listen(serve.bind, serve.port)?);
    }
    if let Some(port) = serve.tls_port {
        builder = builder
            .https_listener(listen(serve.bind, port)?)
            .tls(tls_config(serve));
    }
    let server = builder.build()?;
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
    runtime.block_on(server.serve(async {
        // If signals are unavailable, the server keeps running until it's killed.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }))?;
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

/// Binds a socket, naming the address in the error because a server may bind several.
fn listen(address: IpAddr, port: u16) -> Result<TcpListener> {
    TcpListener::bind((address, port))
        .with_context(|| format!("couldn't listen on {address}:{port}"))
}

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
