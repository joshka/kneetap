//! The `kneetap` binary, run as a user would.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_kneetap");

/// A running `kneetap serve`, stopped when dropped.
struct Serve {
    child: Child,
    address: String,
}

impl Serve {
    fn start(bind: &str, args: &[&str]) -> Self {
        let mut child = Command::new(BINARY)
            .args(["serve", "--bind", bind, "--port", "0"])
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let address = line.trim().rsplit('/').next().unwrap().to_owned();
        Self { child, address }
    }

    fn port(&self) -> &str {
        self.address.rsplit(':').next().unwrap()
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn kneetap(args: &[&str]) -> Output {
    Command::new(BINARY).args(args).output().unwrap()
}

#[test]
fn healthcheck_finds_a_local_server() {
    let server = Serve::start("127.0.0.1", &[]);

    let output = kneetap(&["healthcheck", "--port", server.port()]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

#[test]
fn healthcheck_follows_an_ipv6_bind_address() {
    let server = Serve::start("::1", &[]);

    let output = kneetap(&["healthcheck", "--bind", "::1", "--port", server.port()]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

#[test]
fn healthcheck_fails_when_nothing_answers() {
    let port = Serve::start("127.0.0.1", &[]).port().to_owned();

    let output = kneetap(&["healthcheck", "--port", &port]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[test]
fn report_prints_what_the_server_saw() {
    let server = Serve::start("127.0.0.1", &["--name", "blue"]);

    let output = kneetap(&["report", &format!("http://{}", server.address)]);

    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.lines().next(), Some("Name: blue"));
}

#[test]
fn kt_is_the_same_program() {
    let long = kneetap(&["--version"]);

    let short = Command::new(env!("CARGO_BIN_EXE_kt"))
        .arg("--version")
        .output()
        .unwrap();

    assert_eq!(short.stdout, long.stdout);
}

#[test]
fn report_reaches_https_with_a_generated_certificate() {
    let server = Serve::start("127.0.0.1", &["--tls-port", "0", "--no-http"]);

    let report = kneetap(&[
        "report",
        &format!("https://{}", server.address),
        "--insecure",
    ]);

    assert_eq!(report.status.code(), Some(0));
    let text = String::from_utf8(report.stdout).unwrap();
    let session = text.lines().find(|line| line.starts_with("TLS: "));
    assert_eq!(
        session,
        Some("TLS: TLSv1_3 TLS13_AES_256_GCM_SHA384 (absent)")
    );
}

#[test]
fn report_refuses_an_untrusted_certificate_by_default() {
    let server = Serve::start("127.0.0.1", &["--tls-port", "0", "--no-http"]);

    let report = kneetap(&["report", &format!("https://{}", server.address)]);

    assert_eq!(report.status.code(), Some(1));
}
