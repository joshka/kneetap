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

#[cfg(unix)]
#[test]
fn running_out_of_descriptors_does_not_stop_the_server() {
    use std::net::TcpStream;

    let script = "ulimit -n 40; exec \"$0\" serve --bind 127.0.0.1 --port 0";
    let mut child = Command::new("/bin/sh")
        .args(["-c", script, BINARY])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address = line.trim().rsplit('/').next().unwrap().to_owned();
    let mut server = Serve { child, address };

    let held: Vec<_> = (0..60)
        .filter_map(|_| TcpStream::connect(&server.address).ok())
        .collect();
    drop(held);
    std::thread::sleep(std::time::Duration::from_millis(1500));

    assert_eq!(server.child.try_wait().unwrap(), None, "the server exited");
    let health = kneetap(&[
        "healthcheck",
        "--port",
        server.port(),
        "--bind",
        "127.0.0.1",
    ]);
    assert!(health.status.success(), "{health:?}");
}

/// Sends a signal to the server while a slow request runs, returning the response and exit status.
#[cfg(unix)]
fn stop_during_a_request(signal: &str) -> (String, std::process::ExitStatus) {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let mut server = Serve::start("127.0.0.1", &[]);
    let mut stream = TcpStream::connect(&server.address).unwrap();
    stream
        .write_all(b"GET /?wait=500ms HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));

    let pid = server.child.id().to_string();
    let sent = Command::new("/bin/kill").args([signal, &pid]).status();
    assert!(sent.unwrap().success());

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    (response, server.child.wait().unwrap())
}

#[cfg(unix)]
#[rstest::rstest]
#[case("-TERM")]
#[case("-INT")]
fn a_signal_lets_the_request_in_progress_finish(#[case] signal: &str) {
    let (response, status) = stop_during_a_request(signal);

    assert_eq!(response.lines().next(), Some("HTTP/1.1 200 OK"));
    assert!(status.success(), "{status}");
}
