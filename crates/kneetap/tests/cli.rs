//! The `kneetap` binary, run as a user would.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
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

/// Starts `kneetap serve --verbose` with its stderr piped, returning the process and its address.
fn serve_verbose(args: &[&str]) -> (Child, String) {
    let mut child = Command::new(BINARY)
        .args(["serve", "--verbose", "--bind", "127.0.0.1", "--port", "0"])
        .args(args)
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address = line.trim().rsplit('/').next().unwrap().to_owned();
    (child, address)
}

/// Sends `bytes` to `address` and returns everything the server sends before closing.
fn exchange(address: &str, bytes: &[u8]) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(bytes).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

const GET_BENCH: &[u8] = b"GET /bench HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";

#[test]
fn verbose_logs_each_request_to_stderr() {
    let (mut child, address) = serve_verbose(&["--log-format", "json"]);

    exchange(&address, GET_BENCH);

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let event = stderr.lines().find(|line| line.contains("request.handled"));
    let event: serde_json::Value = serde_json::from_str(event.unwrap_or("{}")).unwrap();
    assert_eq!(event["fields"]["path"], "/bench", "{stderr}");
}

#[test]
fn an_invalid_log_filter_is_reported_as_a_log_event() {
    let mut child = Command::new(BINARY)
        .args(["serve", "--log-format", "json"])
        .args(["--bind", "127.0.0.1", "--port", "0"])
        .env("RUST_LOG", "kneetap=nonsense")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();

    let stderr = String::from_utf8(output.stderr).unwrap();
    let event: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(event["fields"]["message"], "telemetry.filter.rejected");
}

#[test]
fn verbose_leaves_out_http2_frames() {
    let (mut child, address) = serve_verbose(&[]);

    let mut stream = TcpStream::connect(&address).unwrap();
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
        .unwrap();
    stream.read_exact(&mut [0; 9]).unwrap();

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
}

#[test]
fn logs_written_to_a_pipe_have_no_color() {
    let (mut child, address) = serve_verbose(&[]);

    exchange(&address, GET_BENCH);

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("request.handled"), "{stderr}");
    assert_eq!(stderr.find('\x1b'), None, "{stderr}");
}

#[cfg(unix)]
#[test]
fn a_closed_log_pipe_does_not_stop_the_server() {
    let (mut child, address) = serve_verbose(&[]);
    drop(child.stderr.take());
    let mut server = Serve { child, address };

    let response = exchange(&server.address, GET_BENCH);

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert_eq!(server.child.try_wait().unwrap(), None, "the server exited");
}
