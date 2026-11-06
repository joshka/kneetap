//! A client for [kneetap] servers, which show what actually reaches a backend.
//!
//! Use it to fetch a typed report of a request, or to check a server's health from a script or a
//! container probe.
//!
//! ```no_run
//! use kneetap_client::Client;
//!
//! # async fn example() -> Result<(), kneetap_client::Error> {
//! let client = Client::builder().build()?;
//! let report = client.report("http://127.0.0.1:8080/api").await?;
//! println!("answered by {:?}", report.hostname);
//! # Ok(())
//! # }
//! ```
//!
//! This crate supports the `kneetap` command and has no stability promise before 1.0.
//!
//! [kneetap]: https://github.com/joshka/kneetap

mod client;
mod error;
mod tls;

pub use client::{Client, ClientBuilder};
pub use error::Error;
pub use kneetap_api::Report;
