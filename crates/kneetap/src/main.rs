//! The `kneetap` command.

use std::net::IpAddr;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
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
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve(serve) => run_server(&serve),
    }
}

/// Binds before starting the runtime, so a failure to bind is reported without starting threads.
fn run_server(serve: &Serve) -> Result<()> {
    let mut builder = Server::builder();
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
