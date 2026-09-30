//! End-to-end tests: run the real binary and speak HTTP over a loopback socket.
//!
//! Every request is raw text; HTTP/1.1 requests carry a `Host` header because
//! the server rejects 1.1 requests without exactly one valid `Host`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Read/write timeout for the test client socket.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

/// A running server child process plus the pipe its access log is written to.
struct Server {
    /// The spawned binary.
    child: Child,
    /// The ephemeral port it actually bound.
    port: u16,
    /// The full startup line, e.g.
    /// `helloworld listening on http://127.0.0.1:12345 (language: en)`.
    startup: String,
    /// Retained reader over the child's stdout, positioned after the startup line.
    stdout: BufReader<ChildStdout>,
}

impl Server {
    /// Spawn the binary with `--port 0` plus `extra_args`, and parse its startup line.
    ///
    /// Panics with the child's stderr text if the startup line is missing.
    fn start(extra_args: &[&str]) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_helloworld"));
        command.arg("--port").arg("0");
        command.args(extra_args);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn helloworld");
        let stdout = child.stdout.take().expect("child stdout");
        let mut stdout = BufReader::new(stdout);
        let mut line = String::new();
        match stdout.read_line(&mut line) {
            Ok(n) if n > 0 => match parse_startup_port(&line) {
                Some(port) => Server {
                    child,
                    port,
                    startup: line.trim_end().to_string(),
                    stdout,
                },
                None => panic!("unexpected startup line: {line:?}"),
            },
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("server printed no startup line; stderr: {stderr}");
            }
        }
    }

    /// Read one access-log line from the child's stdout.
    fn next_log_line(&mut self) -> String {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .expect("read access log line");
        assert!(n > 0, "server stdout closed before an access log line");
        line
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extract the port from `helloworld listening on http://127.0.0.1:<port> (language: ...)`.
fn parse_startup_port(line: &str) -> Option<u16> {
    let rest = line.split_once("http://")?.1;
    let addr = rest.split_whitespace().next()?;
    addr.rsplit_once(':')?.1.parse().ok()
}

/// Send `raw` to `127.0.0.1:port` and return the whole response as text.
///
/// The server answers with `Connection: close`, so `read_to_end` ends at EOF.
fn request(port: u16, raw: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream.write_all(raw.as_bytes()).expect("write request");
    stream.flush().unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read response");
    String::from_utf8(buf).expect("response is UTF-8")
}

/// Send `raw`, half-close the write side, and read the response to EOF.
///
/// The half-close makes the server see EOF mid-request, so a short body fails.
fn request_half_close(port: u16, raw: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream.write_all(raw.as_bytes()).expect("write request");
    stream.flush().unwrap();
    stream.shutdown(Shutdown::Write).expect("shutdown write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read response");
    String::from_utf8(buf).expect("response is UTF-8")
}

/// The body of `response`, i.e. everything after the blank line.
fn body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

/// The value of `name` (case-insensitive) in the response head, if present.
fn header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    let head = response.split("\r\n\r\n").next().unwrap_or("");
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The status line of `response`.
fn status(response: &str) -> &str {
    response.lines().next().unwrap_or("")
}

/// Run the binary to completion and return `(exit code, stdout, stderr)`.
fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_helloworld"))
        .args(args)
        .output()
        .expect("run helloworld");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn version_flag_prints_version_and_exits_zero() {
    let (code, stdout, _) = run(&["--version"]);
    assert_eq!(code, 0);
    assert_eq!(
        stdout,
        format!("helloworld {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Usage: helloworld"),
        "usage text missing from {stdout:?}"
    );
    for expected in [
        "-l, --lang",
        "-t, --text",
        "-p, --port",
        "--host",
        "--list-langs",
        "-V, --version",
        "-h, --help",
        "default: 21212",
        "max 8192 bytes",
    ] {
        assert!(
            stdout.contains(expected),
            "usage missing {expected:?}: {stdout:?}"
        );
    }
    for code in [
        "ar", "cs", "da", "de", "el", "en", "es", "fi", "fr", "he", "it", "ja", "ko", "nb", "nl",
        "pl", "pt", "ru", "sv", "tr", "uk", "zh",
    ] {
        assert!(
            stdout.contains(code),
            "usage missing language code {code:?}: {stdout:?}"
        );
    }
}

#[test]
fn list_langs_prints_the_catalog() {
    let (code, stdout, _) = run(&["--list-langs"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 22, "expected 22 locales, got {stdout:?}");
    let codes: Vec<&str> = lines
        .iter()
        .map(|line| line.split_once('\t').expect("tab-separated row").0)
        .collect();
    assert_eq!(
        codes,
        [
            "en", "nl", "de", "fr", "es", "it", "pt", "pl", "sv", "da", "fi", "nb", "ru", "uk",
            "ja", "zh", "ko", "ar", "he", "tr", "el", "cs"
        ],
        "unexpected catalog order: {stdout:?}"
    );
    for line in lines {
        let (code, name) = line.split_once('\t').expect("tab-separated row");
        assert!(
            !code.is_empty() && !name.is_empty(),
            "empty field in {line:?}"
        );
    }
}

#[test]
fn unknown_flag_is_a_usage_error() {
    let (code, stdout, stderr) = run(&["--bogus"]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "unexpected stdout {stdout:?}");
    assert!(!stderr.is_empty(), "expected an error message on stderr");
}

#[test]
fn unsupported_language_is_a_usage_error() {
    let (code, _, stderr) = run(&["--lang", "klingon"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("supported codes:"),
        "stderr should list supported codes: {stderr:?}"
    );
    assert!(
        stderr.contains("nl"),
        "stderr should list codes: {stderr:?}"
    );
}

#[test]
fn get_root_defaults_to_english() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(header(&response, "Content-Length"), Some("14"));
    assert_eq!(header(&response, "Content-Language"), Some("en"));
    assert_eq!(
        header(&response, "Vary"),
        Some("Accept-Language, Accept-Encoding")
    );
    assert_eq!(body(&response), "Hello, World!\n");
}

#[test]
fn accept_language_is_negotiated() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: de-DE,de;q=0.9\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(header(&response, "Content-Language"), Some("de"));
    assert_eq!(body(&response), "Hallo, Welt!\n");
}

#[test]
fn unacceptable_accept_language_is_406() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: en;q=0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 406 Not Acceptable");
    assert_eq!(body(&response), "not acceptable\n");
    assert_eq!(
        header(&response, "Vary"),
        Some("Accept-Language, Accept-Encoding")
    );
    assert_eq!(header(&response, "Content-Language"), None);
}

#[test]
fn identity_refusal_is_406() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity;q=0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 406 Not Acceptable");
    assert_eq!(body(&response), "not acceptable\n");
    assert_eq!(header(&response, "Vary"), Some("Accept-Encoding"));
    assert_eq!(header(&response, "Content-Language"), None);
}

#[test]
fn encoding_that_accepts_identity_is_200() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "Hello, World!\n");
}

#[test]
fn lang_flag_wins_over_accept_language() {
    let server = Server::start(&["--lang", "nl"]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: ru\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(body(&response), "Hallo, wereld!\n");
    assert_eq!(header(&response, "Content-Language"), Some("nl"));
    assert_eq!(header(&response, "Vary"), Some("Accept-Encoding"));
}

#[test]
fn text_flag_replaces_the_greeting() {
    let server = Server::start(&["--text", "Hi there"]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: de\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(body(&response), "Hi there\n");
    assert_eq!(header(&response, "Content-Language"), None);
    assert_eq!(header(&response, "Vary"), Some("Accept-Encoding"));
}

#[test]
fn startup_line_reports_the_language() {
    let server = Server::start(&["--lang", "nl"]);
    assert_eq!(
        server.startup,
        format!(
            "helloworld listening on http://127.0.0.1:{} (language: nl)",
            server.port
        )
    );

    let server = Server::start(&["--text", "hi"]);
    assert_eq!(
        server.startup,
        format!(
            "helloworld listening on http://127.0.0.1:{} (language: custom (--text))",
            server.port
        )
    );
}

#[test]
fn empty_text_is_served() {
    let server = Server::start(&["--text", ""]);
    assert!(
        server.startup.contains("(language: custom (--text))"),
        "unexpected startup line: {:?}",
        server.startup
    );
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(header(&response, "Content-Length"), Some("1"));
    assert_eq!(body(&response), "\n");
}

#[test]
fn head_returns_headers_without_a_body() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "HEAD / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(header(&response, "Content-Length"), Some("14"));
    assert_eq!(body(&response), "");
}

#[test]
fn head_healthz_has_no_body() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "HEAD /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(header(&response, "Content-Length"), Some("3"));
    assert_eq!(body(&response), "");
}

#[test]
fn head_unknown_path_is_404_without_body() {
    let server = Server::start(&[]);
    let raw = "HEAD /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = request(server.port, raw);
    assert_eq!(status(&response), "HTTP/1.1 404 Not Found");
    assert_eq!(header(&response, "Content-Length"), Some("10"));
    assert_eq!(body(&response), "");

    // A GET of the same request carries that body.
    let get = request(
        server.port,
        "GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&get), "HTTP/1.1 404 Not Found");
    assert_eq!(header(&get, "Content-Length"), Some("10"));
    assert_eq!(body(&get), "not found\n");
}

#[test]
fn head_error_response_has_no_body() {
    let server = Server::start(&[]);
    let raw = format!("HEAD /{}\r\nHost: localhost\r\n\r\n", "a".repeat(9000));
    let response = request(server.port, &raw);
    assert_eq!(status(&response), "HTTP/1.1 414 URI Too Long");
    assert_eq!(header(&response, "Content-Length"), Some("22"));
    assert_eq!(body(&response), "");

    // A GET of the same request carries that body.
    let get = request(
        server.port,
        &format!("GET /{}\r\nHost: localhost\r\n\r\n", "a".repeat(9000)),
    );
    assert_eq!(status(&get), "HTTP/1.1 414 URI Too Long");
    assert_eq!(header(&get, "Content-Length"), Some("22"));
    assert_eq!(body(&get), "request line too long\n");

    // An oversized header block is the same parity at `Content-Length: 18`.
    let head = request(
        server.port,
        &format!(
            "HEAD / HTTP/1.1\r\nHost: localhost\r\nX: {}\r\n\r\n",
            "a".repeat(9000)
        ),
    );
    assert_eq!(
        status(&head),
        "HTTP/1.1 431 Request Header Fields Too Large"
    );
    assert_eq!(header(&head, "Content-Length"), Some("18"));
    assert_eq!(body(&head), "");
    let get = request(
        server.port,
        &format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nX: {}\r\n\r\n",
            "a".repeat(9000)
        ),
    );
    assert_eq!(status(&get), "HTTP/1.1 431 Request Header Fields Too Large");
    assert_eq!(header(&get, "Content-Length"), Some("18"));
    assert_eq!(body(&get), "headers too large\n");
}

#[test]
fn over_long_method_is_501() {
    let server = Server::start(&[]);

    // With no separator in the line, the method token alone exceeds the limit,
    // so it is longer than any method this server implements.
    let response = request(server.port, &format!("{}\r\n", "X".repeat(9000)));
    assert_eq!(status(&response), "HTTP/1.1 501 Not Implemented");
    assert_eq!(body(&response), "not implemented\n");

    // A complete method with an over-long target is still a URI that is too long.
    let response = request(
        server.port,
        &format!("GET /{}\r\nHost: localhost\r\n\r\n", "a".repeat(9000)),
    );
    assert_eq!(status(&response), "HTTP/1.1 414 URI Too Long");
    assert_eq!(body(&response), "request line too long\n");
}

#[test]
fn unknown_path_is_404() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 404 Not Found");
    assert_eq!(header(&response, "Content-Language"), None);
}

#[test]
fn post_is_405_with_allow_header() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 405 Method Not Allowed");
    assert_eq!(header(&response, "Allow"), Some("GET, HEAD"));
}

#[test]
fn unrecognized_method_is_501() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "FOO / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 501 Not Implemented");
    assert_eq!(header(&response, "Allow"), None);
    assert_eq!(body(&response), "not implemented\n");
}

#[test]
fn comma_separated_content_length_is_served() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5, 5\r\nConnection: close\r\n\r\nhello",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "Hello, World!\n");
}

#[test]
fn conflicting_comma_separated_content_length_is_400() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5, 6\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn transfer_encoding_without_final_chunked_is_400() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: gzip\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn chunked_request_is_served() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(header(&response, "Content-Length"), Some("14"));
    assert_eq!(body(&response), "Hello, World!\n");
}

#[test]
fn chunked_request_ignores_extensions_and_trailers() {
    let server = Server::start(&[]);

    // Chunk extensions are ignored (RFC 9112 7.1.1).
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5;x=1\r\nhello\r\n0;y\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "Hello, World!\n");

    // Trailer fields are read and discarded (RFC 9112 7.1.2).
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\nX-Trailer: v\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "Hello, World!\n");
}

#[test]
fn malformed_chunked_body_is_400() {
    let server = Server::start(&[]);

    // A chunk-size that is not hexadecimal.
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nz\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    assert_eq!(body(&response), "bad request\n");

    // The body ends before the last chunk.
    let response = request_half_close(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    // The chunk data is cut short.
    let response = request_half_close(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    // The chunk data is not followed by a CRLF.
    let response = request_half_close(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhelloX",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn oversized_chunked_body_is_413() {
    let server = Server::start(&[]);

    // 0x1000001 is one octet over the 8 MiB body cap.
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n1000001\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 413 Content Too Large");
    assert_eq!(body(&response), "payload too large\n");

    // A chunk-size that overflows u64 (RFC 9112 7.1).
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nffffffffffffffffff\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 413 Content Too Large");
    assert_eq!(body(&response), "payload too large\n");
}

#[test]
fn chunked_with_content_length_is_400() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    assert_eq!(body(&response), "bad request\n");
}

#[test]
fn unknown_transfer_coding_with_chunked_is_501() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: gzip, chunked\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 501 Not Implemented");
    assert_eq!(body(&response), "not implemented\n");
}

#[test]
fn chunked_transfer_coding_parameters_are_400() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked;q=1\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn http10_with_chunked_transfer_encoding_is_400() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn http11_without_host_is_400() {
    let server = Server::start(&[]);
    let response = request(server.port, "GET / HTTP/1.1\r\nConnection: close\r\n\r\n");
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn invalid_host_is_400() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: user@host\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    assert_eq!(body(&response), "bad request\n");
}

#[test]
fn http12_is_served_as_11() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.2\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "Hello, World!\n");
}

#[test]
fn malformed_version_token_is_400() {
    let server = Server::start(&[]);
    let response = request(server.port, "GET / HTTP/1.\r\nHost: localhost\r\n\r\n");
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    let response = request(server.port, "GET / HTTP/1.x\r\nHost: localhost\r\n\r\n");
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn asterisk_form_is_400() {
    let server = Server::start(&[]);
    let response = request(server.port, "GET * HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn invalid_request_target_is_400() {
    let server = Server::start(&[]);

    // `foo` is neither origin-form, asterisk-form, absolute-form, nor a valid
    // authority-form.
    let response = request(server.port, "GET foo HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    // authority-form is only valid for CONNECT, and `foo` has no port.
    let response = request(
        server.port,
        "CONNECT foo HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    // A well-formed authority-form CONNECT is routed like any other method.
    let response = request(
        server.port,
        "CONNECT example.com:443 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 405 Method Not Allowed");
    assert_eq!(header(&response, "Allow"), Some("GET, HEAD"));
}

#[test]
fn connect_requires_a_valid_host_and_port() {
    let server = Server::start(&[]);

    // An empty port is rejected (RFC 9110 9.3.6).
    let response = request(
        server.port,
        "CONNECT example.com: HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    // An empty host is rejected (RFC 9110 4.2.1).
    let response = request(
        server.port,
        "CONNECT :443 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    // A port that does not fit u16 is rejected.
    let response = request(
        server.port,
        "CONNECT example.com:99999 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    // A well-formed authority-form CONNECT is routed like any other method.
    let response = request(
        server.port,
        "CONNECT example.com:443 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 405 Method Not Allowed");
    assert_eq!(header(&response, "Allow"), Some("GET, HEAD"));

    // An IPv6 literal authority is still well formed.
    let response = request(
        server.port,
        "CONNECT [::1]:443 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 405 Method Not Allowed");
}

#[test]
fn truncated_body_is_400() {
    let server = Server::start(&[]);
    let response = request_half_close(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nab",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn healthz_returns_ok() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "ok\n");
    assert_eq!(header(&response, "Content-Language"), None);
}

#[test]
fn access_log_line_matches_the_contract() {
    let mut server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");

    let line = server.next_log_line();
    let fields: Vec<&str> = line.split(' ').collect();
    assert_eq!(fields.len(), 7, "expected 7 fields: {line:?}");
    let timestamp = fields[0];
    assert!(
        timestamp.len() == 20
            && timestamp.ends_with('Z')
            && timestamp.as_bytes()[4] == b'-'
            && timestamp.as_bytes()[10] == b'T',
        "timestamp is not ISO8601 UTC: {timestamp:?}"
    );
    let peer = fields[1];
    assert!(
        peer == "-" || peer.starts_with("127.0.0.1:"),
        "unexpected peer field: {peer:?}"
    );
    assert!(
        line.contains(" GET / lang=en status=200 dur="),
        "unexpected access log line: {line:?}"
    );
    assert!(line.ends_with("ms\n"), "missing duration: {line:?}");
    let duration = fields[6]
        .strip_prefix("dur=")
        .and_then(|value| value.strip_suffix("ms\n"))
        .expect("dur= field");
    let (whole, fraction) = duration.split_once('.').expect("one decimal digit");
    assert!(
        !whole.is_empty() && whole.bytes().all(|b| b.is_ascii_digit()),
        "duration has no whole part: {line:?}"
    );
    assert_eq!(
        fraction.len(),
        1,
        "duration needs one decimal digit: {line:?}"
    );
    assert!(
        fraction.bytes().all(|b| b.is_ascii_digit()),
        "duration fraction is not a digit: {line:?}"
    );
}

#[test]
fn query_is_stripped_from_the_log() {
    let mut server = Server::start(&[]);
    let response = request(
        server.port,
        "GET /?x=1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");

    let line = server.next_log_line();
    assert!(
        line.contains(" GET / lang=en status=200 "),
        "unexpected access log line: {line:?}"
    );
}

#[test]
fn access_log_records_status_and_language() {
    let mut server = Server::start(&[]);
    request(
        server.port,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let line = server.next_log_line();
    assert!(
        line.contains("- status=200"),
        "unexpected access log line: {line:?}"
    );

    request(
        server.port,
        "GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let line = server.next_log_line();
    assert!(
        line.contains("- status=404"),
        "unexpected access log line: {line:?}"
    );

    request(
        server.port,
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let line = server.next_log_line();
    assert!(
        line.contains("- status=405"),
        "unexpected access log line: {line:?}"
    );

    let mut server = Server::start(&["--text", "hi"]);
    request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let line = server.next_log_line();
    assert!(
        line.contains("lang=custom status=200"),
        "unexpected access log line: {line:?}"
    );
}

#[test]
fn malformed_header_line_is_400() {
    let server = Server::start(&[]);
    let response = request(server.port, "GET / HTTP/1.1\r\nHost : localhost\r\n\r\n");
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn absolute_form_target_is_served() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET http://localhost/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "Hello, World!\n");
}

#[test]
fn http10_ignores_expect_100_continue() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.0\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\nabc",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "Hello, World!\n");
}

#[test]
fn expect_list_members_are_combined() {
    let server = Server::start(&[]);

    // A member other than `100-continue` makes the whole list unrecognized.
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue, nope\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 417 Expectation Failed");
    assert_eq!(body(&response), "expectation failed\n");

    // Repeated field lines combine in order, so both members are recognized.
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nExpect: 100-continue\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    );
    assert!(
        response.starts_with("HTTP/1.1 100 Continue\r\n\r\n"),
        "response was {response:?}"
    );
    let final_response = response
        .strip_prefix("HTTP/1.1 100 Continue\r\n\r\n")
        .expect("100 Continue prefix");
    assert_eq!(status(final_response), "HTTP/1.1 200 OK");
    assert_eq!(body(final_response), "Hello, World!\n");

    // An empty list element is ignored, so no member remains and the
    // expectation is ignored; the body is read as usual with no interim reply.
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nExpect: ,\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "Hello, World!\n");
    assert!(
        !response.starts_with("HTTP/1.1 100 Continue"),
        "unexpected interim response: {response:?}"
    );
}

#[test]
fn slow_request_body_times_out() {
    let server = Server::start(&[]);
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .expect("write timeout");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n")
        .expect("write head");

    // Trickle the body so only the 10s request deadline can answer.
    let started = Instant::now();
    let mut response = Vec::new();
    while started.elapsed() < Duration::from_secs(12) {
        thread::sleep(Duration::from_millis(1000));
        let mut scratch = [0_u8; 1024];
        if let Ok(read) = stream.read(&mut scratch) {
            if read > 0 {
                response.extend_from_slice(&scratch[..read]);
                break;
            }
        }
        if stream.write_all(b"x").is_err() {
            break;
        }
    }
    assert!(
        String::from_utf8_lossy(&response).starts_with("HTTP/1.1 408"),
        "expected 408, got {:?} after {:?}",
        String::from_utf8_lossy(&response),
        started.elapsed()
    );
}

#[test]
fn over_capacity_returns_503() {
    let mut server = Server::start(&[]);
    // Track the documented connection cap by probing after each idle connection
    // rather than hardcoding it.
    let mut idle: Vec<TcpStream> = Vec::new();
    let mut response = String::new();
    for _ in 0..512 {
        idle.push(TcpStream::connect(("127.0.0.1", server.port)).expect("connect"));
        response = request(
            server.port,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        if status(&response) == "HTTP/1.1 503 Service Unavailable" {
            break;
        }
    }
    assert_eq!(status(&response), "HTTP/1.1 503 Service Unavailable");
    assert_eq!(header(&response, "Retry-After"), Some("1"));
    assert_eq!(header(&response, "Vary"), Some("Accept-Encoding"));
    // The rejection is bodyless, so it is also correct for `HEAD`.
    assert_eq!(body(&response), "");
    // Every probe before the rejection is logged too, so drain until the 503 line.
    let mut line = server.next_log_line();
    for _ in 0..512 {
        if line.contains("status=503") {
            break;
        }
        line = server.next_log_line();
    }
    assert!(line.contains("status=503"), "was {line:?}");
    drop(idle);
}

#[test]
fn only_ows_is_trimmed_from_field_values() {
    let server = Server::start(&[]);
    // Optional whitespace is SP and HTAB only (RFC 9110 5.6.3), so a non-ASCII
    // space stays in the field value and the framing field rejects it.
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: \u{a0}5\r\nConnection: close\r\n\r\nhello",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: \u{a0}evil.example\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: \t5 \t\r\nConnection: close\r\n\r\nhello",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
}

#[test]
fn unicode_whitespace_in_the_request_line_is_400() {
    let server = Server::start(&[]);
    // The RFC 9112 3 separators are SP, HTAB, VT, FF and bare CR; a non-ASCII
    // space is part of the token, so the line is not a valid request line.
    let response = request(
        server.port,
        "GET\u{a0}/\u{a0}HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    assert_eq!(body(&response), "bad request\n");
}

#[test]
fn fragment_in_the_target_is_400() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET /#f HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn multi_digit_version_is_400() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.10\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn absolute_form_is_routed_by_path_for_every_scheme() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET ftp://example.com/healthz HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), "ok\n");
}

#[test]
fn http_absolute_form_requires_a_host() {
    let server = Server::start(&[]);

    // An empty host identifier is rejected (RFC 9110 4.2.1).
    for target in [
        "http:///healthz",
        "http://",
        "http:/healthz",
        "http://:80/healthz",
    ] {
        let response = request(
            server.port,
            &format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(
            status(&response),
            "HTTP/1.1 400 Bad Request",
            "target {target:?}"
        );
    }

    // Userinfo is an error in an http URI (RFC 9110 4.2.4).
    let response = request(
        server.port,
        "GET http://user@host/healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");

    // An unbalanced IPv6 literal bracket is rejected.
    let response = request(
        server.port,
        "GET http://[::1/x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
}

#[test]
fn valid_absolute_forms_are_served() {
    let server = Server::start(&[]);

    // A bracketed host and an empty (defaulted) port stay valid.
    for target in [
        "http://[::1]/healthz",
        "http://example.com:/healthz",
        "http://localhost/",
    ] {
        let response = request(
            server.port,
            &format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(status(&response), "HTTP/1.1 200 OK", "target {target:?}");
    }

    // A non-http scheme keeps the lenient generic absolute-URI handling.
    let response = request(
        server.port,
        "GET ftp:///x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status(&response), "HTTP/1.1 404 Not Found");
}

#[test]
fn expect_member_with_parameters_is_recognized() {
    let server = Server::start(&[]);
    let response = request(
        server.port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue;q=1\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    );
    assert!(
        response.starts_with("HTTP/1.1 100 Continue\r\n\r\n"),
        "response was {response:?}"
    );
    let final_response = response
        .strip_prefix("HTTP/1.1 100 Continue\r\n\r\n")
        .expect("100 Continue prefix");
    assert_eq!(status(final_response), "HTTP/1.1 200 OK");
    assert_eq!(body(final_response), "Hello, World!\n");
}

#[test]
fn marker_with_a_value_is_a_usage_error() {
    let (code, stdout, stderr) = run(&["--=nl"]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "unexpected stdout {stdout:?}");
    assert!(
        stderr.contains("unknown argument '--=nl'"),
        "unexpected stderr {stderr:?}"
    );
}

#[test]
fn brackets_around_an_ipv4_host_are_a_usage_error() {
    let (code, _, stderr) = run(&["--host", "[127.0.0.1]"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("invalid host '[127.0.0.1]'"),
        "unexpected stderr {stderr:?}"
    );
}

#[test]
fn bind_failure_exits_one() {
    let server = Server::start(&[]);
    let port = server.port.to_string();
    let (code, stdout, stderr) = run(&["--port", &port]);
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "unexpected stdout {stdout:?}");
    assert!(!stderr.is_empty(), "expected an error message on stderr");
}
