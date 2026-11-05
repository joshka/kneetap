//! Report types for [kneetap], a server that shows what actually reaches a backend.
//!
//! kneetap runs behind a proxy, load balancer, or service mesh and answers each request with a
//! report: which instance handled it, and what the request looked like when it arrived. This crate
//! defines that report, so a program can read one without depending on the server.
//!
//! [`Report`] is the contract between the two, and its JSON form is what `/api` returns.
//!
//! This crate supports the `kneetap` command and has no stability promise before 1.0.
//!
//! [kneetap]: https://github.com/joshka/kneetap

mod report;

pub use report::{BodyEncoding, Report};
