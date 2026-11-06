use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use kneetap_client::Client;
use kneetap_server::Server;

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
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Base URL of the server
    url: String,

    /// Print JSON instead of text
    #[arg(long)]
    json: bool,
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
    let client = Client::builder().timeout(check.timeout).build()?;
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
    let client = Client::builder().build()?;
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
    let mut builder = Server::builder();
    if serve.enable_env {
        builder = builder.enable_env();
    }
    if let Some(name) = &serve.name {
        builder = builder.name(name);
    }
    let server = builder
        .bind((serve.bind, serve.port))
        .with_context(|| format!("couldn't listen on {}:{}", serve.bind, serve.port))?;
    println!("listening on http://{}", server.local_addr());
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(server.serve(async {
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
