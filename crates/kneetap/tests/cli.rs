//! The `kneetap` binary, run as a user would.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

const BINARY: &str = env!("CARGO_BIN_EXE_kneetap");

/// A running `kneetap serve`, stopped when dropped.
struct Serve {
    child: Child,
    scheme: String,
    address: String,
}

/// Turns HTTPS off unless the test wants it, because the default port can only be held once.
fn tls_default(args: &[&str]) -> &'static [&'static str] {
    if args.contains(&"--tls-port") {
        &[]
    } else {
        &["--no-tls"]
    }
}

impl Serve {
    fn start(bind: &str, args: &[&str]) -> Self {
        let child = Command::new(BINARY)
            .args(["serve", "--bind", bind, "--port", "0"])
            .args(tls_default(args))
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Self::attach(child)
    }

    /// Waits for a started server to print its first URL.
    fn attach(mut child: Child) -> Self {
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let (scheme, address) = line.trim().rsplit_once("://").unwrap();
        let scheme = scheme.rsplit(' ').next().unwrap().to_owned();
        let address = address.to_owned();
        Self {
            child,
            scheme,
            address,
        }
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
fn healthcheck_finds_an_https_only_server() {
    let server = Serve::start("127.0.0.1", &["--tls-port", "0", "--no-http"]);

    let output = kneetap(&["healthcheck", "--no-http", "--tls-port", server.port()]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

#[test]
fn healthcheck_ignores_a_server_with_another_name() {
    let server = Serve::start("127.0.0.1", &["--name", "other"]);

    let output = kneetap(&["healthcheck", "--port", server.port(), "--name", "target"]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

/// Starts an HTTPS-only server that requires client certificates, keeping its authority's file.
///
/// `label` keeps the file apart from those of tests running in the same process.
fn client_auth_server(label: &str) -> (Serve, PathBuf) {
    let authority = rcgen::generate_simple_self_signed(["authority".to_owned()]).unwrap();
    let file = format!("kneetap-ca-{}-{label}.pem", std::process::id());
    let path = std::env::temp_dir().join(file);
    std::fs::write(&path, authority.cert.pem()).unwrap();
    let args = ["--tls-port", "0", "--no-http", "--cacert"];
    let server = Serve::start(
        "127.0.0.1",
        &[&args[..], &[path.to_str().unwrap()]].concat(),
    );
    (server, path)
}

#[rstest::rstest]
#[case(&["--server-cacert", "/any"], 0)]
#[case(&[], 1)]
fn healthcheck_accepts_a_certificate_demand_only_when_told_to_expect_it(
    #[case] args: &[&str],
    #[case] code: i32,
) {
    let (server, path) = client_auth_server(&format!("demand-{code}"));

    let check = ["healthcheck", "--no-http", "--tls-port", server.port()];
    let output = kneetap(&[&check[..], args].concat());

    let _ = std::fs::remove_file(path);
    assert_eq!(output.status.code(), Some(code), "{output:?}");
}

#[test]
fn a_certificate_demand_does_not_excuse_an_unhealthy_server() {
    let (tls, path) = client_auth_server("unhealthy");
    let http = Serve::start("127.0.0.1", &[]);
    let mut stream = TcpStream::connect(&http.address).unwrap();
    let update = "POST /health HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\
        Content-Type: application/json\r\nContent-Length: 3\r\n\r\n503";
    stream.write_all(update.as_bytes()).unwrap();
    stream.read_to_end(&mut Vec::new()).unwrap();

    let output = kneetap(&[
        "healthcheck",
        "--port",
        http.port(),
        "--tls-port",
        tls.port(),
        "--server-cacert",
        "/any",
    ]);

    let _ = std::fs::remove_file(path);
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

    let script = "ulimit -n 40; exec \"$0\" serve --bind 127.0.0.1 --port 0 --no-tls";
    let child = Command::new("/bin/sh")
        .args(["-c", script, BINARY])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = Serve::attach(child);

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
        .args([
            "serve",
            "--verbose",
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--no-tls",
        ])
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
        .args(["--bind", "127.0.0.1", "--port", "0", "--no-tls"])
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
    let scheme = "http".to_owned();
    let mut server = Serve {
        child,
        scheme,
        address,
    };

    let response = exchange(&server.address, GET_BENCH);

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert_eq!(server.child.try_wait().unwrap(), None, "the server exited");
}

/// Starts a server and returns its report from `/api`, for checking what it worked around.
fn status_of(args: &[&str]) -> String {
    let server = Serve::start("127.0.0.1", args);
    let report = kneetap(&[
        "report",
        "--json",
        "--insecure",
        &format!("https://{}", server.address),
    ]);
    String::from_utf8(report.stdout).unwrap()
}

#[test]
fn an_unusable_certificate_falls_back_to_a_generated_one() {
    let args = [
        "--no-http",
        "--tls-port",
        "0",
        "--cert",
        "/missing",
        "--key",
        "/missing",
    ];

    let report = status_of(&args);

    let report: serde_json::Value = serde_json::from_str(&report).unwrap();
    let degradations = &report["serverStatus"]["degradations"];
    assert_eq!(
        *degradations,
        serde_json::json!(["server_identity_fallback"])
    );
    assert_eq!(report["tls"]["certificateSource"], "generated");
}

#[test]
fn strict_refuses_the_generated_certificate_fallback() {
    let output = kneetap(&[
        "serve",
        "--strict",
        "--bind",
        "127.0.0.1",
        "--port",
        "0",
        "--tls-port",
        "0",
        "--cert",
        "/missing",
        "--key",
        "/missing",
    ]);

    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn a_broken_client_authority_never_serves_https_without_it() {
    let output = kneetap(&[
        "serve",
        "--no-http",
        "--bind",
        "127.0.0.1",
        "--tls-port",
        "0",
        "--cacert",
        "/missing",
    ]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
}

#[test]
fn the_default_bind_serves_ipv6_and_ipv4_on_one_port() {
    let child = Command::new(BINARY)
        .args(["serve", "--port", "0", "--no-tls"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let server = Serve::attach(child);

    // Some containers and CI runners have no IPv6 loopback to check.
    let has_ipv6 = std::net::TcpListener::bind("[::1]:0").is_ok();
    let binds = if has_ipv6 {
        &["127.0.0.1", "::1"][..]
    } else {
        &["127.0.0.1"][..]
    };
    for bind in binds {
        let health = kneetap(&["healthcheck", "--bind", bind, "--port", server.port()]);
        assert!(health.status.success(), "{bind}: {health:?}");
    }
}

/// Returns the name a server reports when configured with `KNEETAP_NAME` and these arguments.
fn name_with_environment(args: &[&str]) -> String {
    let child = Command::new(BINARY)
        .args(["serve", "--bind", "127.0.0.1", "--port", "0", "--no-tls"])
        .args(args)
        .env("KNEETAP_NAME", "from-environment")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let server = Serve::attach(child);
    let report = kneetap(&["report", &format!("http://{}", server.address)]);
    let text = String::from_utf8(report.stdout).unwrap();
    text.lines().next().unwrap().to_owned()
}

#[rstest::rstest]
#[case(&[], "Name: from-environment")]
#[case(&["--name", "from-flag"], "Name: from-flag")]
fn flags_take_priority_over_environment_variables(#[case] args: &[&str], #[case] name: &str) {
    assert_eq!(name_with_environment(args), name);
}

/// Returns the scheme of the first URL a server prints when configured with these variables.
fn first_scheme_with_environment(variables: &[(&str, &str)]) -> String {
    let child = Command::new(BINARY)
        .args([
            "serve",
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--tls-port",
            "0",
        ])
        .envs(variables.iter().copied())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let server = Serve::attach(child);
    server.scheme.clone()
}

#[rstest::rstest]
#[case(&[("KNEETAP_NO_HTTP", "1")], "https")]
#[case(&[("KNEETAP_NO_HTTP", "true")], "https")]
#[case(&[("KNEETAP_NO_HTTP", "0")], "http")]
#[case(&[("KNEETAP_NO_HTTP", "false"), ("KNEETAP_NO_TLS", "false")], "http")]
fn flags_set_in_the_environment_can_be_on_or_off(
    #[case] variables: &[(&str, &str)],
    #[case] scheme: &str,
) {
    assert_eq!(first_scheme_with_environment(variables), scheme);
}

#[test]
fn turning_off_both_listeners_is_an_error() {
    let output = kneetap(&["serve", "--no-http", "--no-tls"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
}
