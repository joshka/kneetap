//! The implementation of the `kneetap` and `kt` commands.
//!
//! This is a library only so that both binaries can share it. Embed `kneetap-server` or use
//! `kneetap-client` instead.

mod cli;
mod telemetry;

pub use cli::run;
