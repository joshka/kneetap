use std::io::{IsTerminal, Stderr};
use std::sync::OnceLock;

use clap::ValueEnum;
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::writer::EitherWriter;

/// The variable holding log filter directives.
const FILTER_VARIABLE: &str = "KNEETAP_LOG";

/// How many lines wait for a slow stderr before new ones are dropped, which bounds their memory.
const BUFFERED_LINES: usize = 1024;

/// The background writer, once a server has started one.
static BACKGROUND: OnceLock<NonBlocking> = OnceLock::new();

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
/// `KNEETAP_LOG` takes priority over `verbosity`, which only raises kneetap's own events because
/// HTTP/2 logs every frame at debug. Colors are for terminals, so captured logs stay plain. Failed
/// writes are ignored, because a closed log pipe shouldn't stop a server. An invalid `KNEETAP_LOG`
/// is replaced by the default and logged as an event, because the filter parser's own report goes
/// straight to stderr, unformatted and able to panic.
pub fn init(format: LogFormat, verbosity: Verbosity) {
    let default = match verbosity {
        Verbosity::Normal => "info",
        Verbosity::Verbose => "info,kneetap_server=debug",
    };
    let directives = std::env::var(FILTER_VARIABLE).unwrap_or_else(|_| default.to_owned());
    let filters = EnvFilter::builder().with_default_directive(LevelFilter::INFO.into());
    let parsed = filters.parse(directives);
    let rejected = parsed.as_ref().err().map(ToString::to_string);
    let filter = parsed.unwrap_or_else(|_| filters.parse_lossy(default));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
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

/// Moves log writes to a background thread, so a blocked stderr can't stall the server.
///
/// Lines beyond [`BUFFERED_LINES`] are dropped. Call this after dropping privileges, because the
/// thread keeps its identity on platforms where a change only affects the calling thread. Dropping
/// the guard writes what's queued, waiting about a second at most.
pub fn write_in_background() -> WorkerGuard {
    let (background, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(BUFFERED_LINES)
        .thread_name("kneetap-logs")
        .finish(std::io::stderr());
    let _ = BACKGROUND.set(background);
    guard
}

/// Returns the background writer if there is one, and otherwise stderr itself.
fn writer() -> EitherWriter<NonBlocking, Stderr> {
    match BACKGROUND.get() {
        Some(background) => EitherWriter::A(background.clone()),
        None => EitherWriter::B(std::io::stderr()),
    }
}
