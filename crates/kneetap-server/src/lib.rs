//! The embeddable server behind [kneetap], which shows what actually reaches a backend.
//!
//! The server answers each request with a report saying which instance handled it and what the
//! request looked like when it arrived. Embed it to put a diagnostic endpoint in your own process
//! or tests; to run one on its own, use the `kneetap` command.
//!
//! Binding is separate from serving, so a process can claim a privileged port and then give up its
//! privileges before it handles any request.
//!
//! ```no_run
//! use kneetap_server::Server;
//!
//! # async fn example() -> Result<(), kneetap_server::Error> {
//! let server = Server::builder().name("blue").bind("127.0.0.1:8080")?;
//! println!("listening on {}", server.local_addr());
//! server.serve(std::future::pending()).await
//! # }
//! ```
//!
//! This crate supports the `kneetap` command and has no stability promise before 1.0.
//!
//! [kneetap]: https://github.com/joshka/kneetap

mod data;
mod error;
mod host;
mod report;
mod routes;
mod server;

pub use error::Error;
pub use server::{Server, ServerBuilder};
