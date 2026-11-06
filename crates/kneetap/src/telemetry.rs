use std::io::IsTerminal;

use clap::ValueEnum;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

/// How log events are written.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum LogFormat {
    /// Human-readable lines
    #[default]
    Text,

    /// One JSON object per line
    Json,
}

/// How much of kneetap's own activity is logged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verbosity {
    /// Server events worth an operator's attention
    Normal,

    /// Those events, plus a line per request
    Verbose,
}

/// Installs the process's log subscriber, writing to stderr so that stdout stays for results.
///
/// `RUST_LOG` takes priority over `verbosity`, which only raises kneetap's own events because
/// HTTP/2 logs every frame at debug. Colors are for terminals, so captured logs stay plain. Failed
/// writes are ignored, because a closed log pipe shouldn't stop a server. An invalid `RUST_LOG` is
/// replaced by the default and logged as an event, because the filter parser's own report goes
/// straight to stderr, unformatted and able to panic.
pub fn init(format: LogFormat, verbosity: Verbosity) {
    let default = match verbosity {
        Verbosity::Normal => "info",
        Verbosity::Verbose => "info,kneetap_server=debug",
    };
    let directives = std::env::var("RUST_LOG").unwrap_or_else(|_| default.to_owned());
    let filters = EnvFilter::builder().with_default_directive(LevelFilter::INFO.into());
    let parsed = filters.parse(directives);
    let rejected = parsed.as_ref().err().map(ToString::to_string);
    let filter = parsed.unwrap_or_else(|_| filters.parse_lossy(default));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .log_internal_errors(false);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
    if let Some(error) = rejected {
        tracing::warn!(%error, "telemetry.filter.rejected");
    }
}
