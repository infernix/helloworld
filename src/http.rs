//! Dependency-free HTTP/1.x server: one thread per connection.
//!
//! The request path is deliberately small and hardened: every line is length
//! bounded, header and body sizes are capped, malformed input is answered with a
//! status code instead of an error, and the response builder is generic over the
//! writer so the whole layer can be unit tested without a socket.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::i18n;

/// Largest accepted request line, excluding the trailing CRLF.
const MAX_REQUEST_LINE: usize = 8 * 1024;

/// Largest accepted single header line, excluding the trailing CRLF.
const MAX_HEADER_LINE: usize = 8 * 1024;

/// Largest accepted total size of the header block.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Largest accepted number of header fields.
const MAX_HEADERS: usize = 100;

/// Largest request body that is drained before the request is rejected.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Largest number of connections served concurrently.
const MAX_CONNECTIONS: usize = 256;

/// Read timeout applied to every accepted connection.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Write timeout applied to every accepted connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause after a failed `accept` before trying again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Maximum number of characters logged per field.
const MAX_LOG_FIELD: usize = 120;

/// `Server` response header value.
const SERVER: &str = concat!("helloworld/", env!("CARGO_PKG_VERSION"));

/// Weekday names for IMF-fixdate formatting, indexed by day of week.
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Month names for IMF-fixdate formatting, indexed by month number minus one.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Runtime configuration derived from the command line.
pub struct Config {
    /// Language forced by `--lang` (already validated/normalized), if any.
    pub lang: Option<&'static str>,
    /// Custom greeting from `--text`; replaces every greeting when present.
    pub text: Option<String>,
}

/// What was served for one request, for logging and tests.
#[derive(Debug, PartialEq, Eq)]
struct RequestInfo {
    /// Request method, or `-` when the request line could not be parsed.
    method: String,
    /// Request path with query and fragment stripped, or `-`.
    path: String,
    /// Language label used for the greeting, or `-`.
    lang: String,
    /// Full status line text, e.g. `200 OK`.
    status: &'static str,
}

/// Accept connections forever, handling each one on its own thread.
///
/// At most [`MAX_CONNECTIONS`] connections are handled at once; further
/// connections receive an immediate `503`. Accept errors back off briefly instead of
/// spinning.
pub fn serve(listener: TcpListener, config: Arc<Config>) {
    let active = Arc::new(AtomicUsize::new(0));
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    reject_busy(stream);
                    continue;
                }
                active.fetch_add(1, Ordering::Relaxed);
                let config = Arc::clone(&config);
                let counter = Arc::clone(&active);
                let spawned = thread::Builder::new()
                    .name("helloworld-conn".to_string())
                    .spawn(move || {
                        let _guard = ActiveGuard(counter);
                        if let Err(err) = handle_connection(stream, &config) {
                            eprintln!("helloworld: connection error: {err}");
                        }
                    });
                if let Err(err) = spawned {
                    active.fetch_sub(1, Ordering::Relaxed);
                    eprintln!("helloworld: failed to spawn handler: {err}");
                }
            }
            Err(err) => {
                eprintln!("helloworld: accept failed: {err}");
                thread::sleep(ACCEPT_BACKOFF);
            }
        }
    }
}

/// Decrements the active-connection counter when the handler thread ends.
struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Answer a connection that arrived while the server was at capacity.
fn reject_busy(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = respond(
        &mut stream,
        "503 Service Unavailable",
        &[("Retry-After", "1")],
        "busy\n",
        false,
    );
}

/// Apply the socket timeouts and serve one connection.
fn handle_connection(stream: TcpStream, config: &Config) -> std::io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    if let Some(info) = handle(&mut reader, &mut writer, config)? {
        log(&info);
    }
    Ok(())
}

/// Outcome of reading one line.
enum ReadLine {
    /// A line without its trailing CRLF.
    Line(String),
    /// The line exceeded the configured limit.
    TooLong,
    /// The input ended before any byte was read.
    Eof,
}

/// Read one CRLF-terminated line, bounded by `limit` bytes excluding the CRLF.
///
/// Invalid UTF-8 is replaced rather than reported, and the caller supplies the
/// scratch buffer so a connection reuses one allocation.
fn read_line<R: BufRead>(
    reader: &mut R,
    limit: usize,
    buf: &mut Vec<u8>,
) -> std::io::Result<ReadLine> {
    buf.clear();
    let read = (&mut *reader)
        .take(limit as u64 + 1)
        .read_until(b'\n', buf)?;
    if read == 0 {
        return Ok(ReadLine::Eof);
    }
    if buf.len() > limit {
        return Ok(ReadLine::TooLong);
    }
    Ok(ReadLine::Line(
        String::from_utf8_lossy(buf)
            .trim_end_matches(['\r', '\n'])
            .to_string(),
    ))
}

/// Parse one request and write the response.
///
/// Returns `None` when the peer closed the connection before sending a request
/// line, and otherwise the description of what was served.
fn handle<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    config: &Config,
) -> std::io::Result<Option<RequestInfo>> {
    let mut buf = Vec::new();

    // Skip any leading blank lines, then read the request line.
    let request_line = loop {
        match read_line(reader, MAX_REQUEST_LINE, &mut buf)? {
            ReadLine::Eof => return Ok(None),
            ReadLine::TooLong => {
                return fail(
                    writer,
                    "-",
                    "-",
                    "414 URI Too Long",
                    "request line too long\n",
                    false,
                );
            }
            ReadLine::Line(line) => {
                if !line.is_empty() {
                    break line;
                }
            }
        }
    };

    let mut parts = request_line.split_whitespace();
    let (method, target, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(target), Some(version), None) => (method, target, version),
        _ => {
            return fail(writer, "-", "-", "400 Bad Request", "bad request\n", false);
        }
    };

    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_graphic()) || target.is_empty() {
        return fail(
            writer,
            method,
            "-",
            "400 Bad Request",
            "bad request\n",
            false,
        );
    }
    let head_only = method == "HEAD";
    if !version.starts_with("HTTP/") {
        return fail(
            writer,
            method,
            target,
            "400 Bad Request",
            "bad request\n",
            head_only,
        );
    }
    if version != "HTTP/1.0" && version != "HTTP/1.1" {
        return fail(
            writer,
            method,
            target,
            "505 HTTP Version Not Supported",
            "http version not supported\n",
            head_only,
        );
    }

    // Read headers until the blank line that ends the header block.
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut header_bytes = 0_usize;
    loop {
        match read_line(reader, MAX_HEADER_LINE, &mut buf)? {
            ReadLine::Eof => break,
            ReadLine::TooLong => {
                return fail(
                    writer,
                    method,
                    target,
                    "431 Request Header Fields Too Large",
                    "headers too large\n",
                    head_only,
                );
            }
            ReadLine::Line(line) => {
                if line.is_empty() {
                    break;
                }
                header_bytes += line.len() + 2;
                if header_bytes > MAX_HEADER_BYTES || headers.len() >= MAX_HEADERS {
                    return fail(
                        writer,
                        method,
                        target,
                        "431 Request Header Fields Too Large",
                        "headers too large\n",
                        head_only,
                    );
                }
                if let Some((name, value)) = line.split_once(':') {
                    let name = name.trim().to_ascii_lowercase();
                    let valid = !name.is_empty()
                        && !name
                            .chars()
                            .any(|c| c.is_ascii_whitespace() || c.is_control());
                    if valid {
                        headers.push((name, value.trim().to_string()));
                    }
                }
            }
        }
    }

    let path = target.split(['?', '#']).next().unwrap_or("").to_string();

    // A body is drained, never buffered, so a chunked request is unsupported.
    if header(&headers, "transfer-encoding").is_some() {
        return fail(
            writer,
            method,
            &path,
            "501 Not Implemented",
            "not implemented\n",
            head_only,
        );
    }

    let mut content_length: Option<u64> = None;
    for (_, value) in headers.iter().filter(|(name, _)| name == "content-length") {
        let parsed = match value.parse::<u64>() {
            Ok(parsed) => parsed,
            Err(_) => {
                return fail(
                    writer,
                    method,
                    &path,
                    "400 Bad Request",
                    "bad request\n",
                    head_only,
                );
            }
        };
        if let Some(previous) = content_length {
            if previous != parsed {
                return fail(
                    writer,
                    method,
                    &path,
                    "400 Bad Request",
                    "bad request\n",
                    head_only,
                );
            }
        } else if parsed > MAX_BODY as u64 {
            return fail(
                writer,
                method,
                &path,
                "413 Payload Too Large",
                "payload too large\n",
                head_only,
            );
        }
        content_length = Some(parsed);
    }

    if header(&headers, "expect").is_some_and(|value| value.eq_ignore_ascii_case("100-continue")) {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        writer.flush()?;
    }

    let len = content_length.unwrap_or(0);
    let mut limited = (&mut *reader).take(len);
    let copied = io::copy(&mut limited, &mut io::sink())?;
    if copied != len {
        return fail(
            writer,
            method,
            &path,
            "400 Bad Request",
            "bad request\n",
            head_only,
        );
    }

    let (status, lang, body, extra): (&'static str, String, String, &[(&str, &str)]) = match method
    {
        "GET" | "HEAD" => match path.as_str() {
            "/" => {
                let (lang, body) = greeting(config, &headers);
                ("200 OK", lang, body, &[])
            }
            "/healthz" => ("200 OK", "-".to_string(), "ok\n".to_string(), &[]),
            _ => (
                "404 Not Found",
                "-".to_string(),
                "not found\n".to_string(),
                &[],
            ),
        },
        _ => (
            "405 Method Not Allowed",
            "-".to_string(),
            "method not allowed\n".to_string(),
            &[("Allow", "GET, HEAD")],
        ),
    };

    reply(
        writer,
        RequestInfo::new(method, &path, &lang, status),
        &body,
        extra,
        head_only,
    )
}

impl RequestInfo {
    /// Describe one request outcome for logging.
    fn new(method: &str, path: &str, lang: &str, status: &'static str) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            lang: lang.to_string(),
            status,
        }
    }
}

/// Write a response and describe it as a [`RequestInfo`].
fn reply<W: Write>(
    writer: &mut W,
    info: RequestInfo,
    body: &str,
    extra: &[(&str, &str)],
    head_only: bool,
) -> std::io::Result<Option<RequestInfo>> {
    respond(writer, info.status, extra, body, head_only)?;
    Ok(Some(info))
}

/// Reject a request before routing, logging `-` as its language.
fn fail<W: Write>(
    writer: &mut W,
    method: &str,
    path: &str,
    status: &'static str,
    body: &str,
    head_only: bool,
) -> std::io::Result<Option<RequestInfo>> {
    reply(
        writer,
        RequestInfo::new(method, path, "-", status),
        body,
        &[],
        head_only,
    )
}

/// Resolve the greeting for a request following the documented precedence:
/// `--text`, then `--lang`, then `Accept-Language`, then the default.
fn greeting(config: &Config, headers: &[(String, String)]) -> (String, String) {
    if let Some(text) = &config.text {
        let mut body = text.clone();
        body.push('\n');
        return ("custom".to_string(), body);
    }
    if let Some(code) = config.lang {
        return (code.to_string(), format!("{}\n", i18n::greeting_for(code)));
    }
    let negotiated = header(headers, "accept-language")
        .and_then(i18n::negotiate)
        .unwrap_or(i18n::DEFAULT_CODE);
    (
        negotiated.to_string(),
        format!("{}\n", i18n::greeting_for(negotiated)),
    )
}

/// First value of the named (lowercase) header, if present.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Serialize one response: status line, fixed headers, extras, then the body.
fn respond<W: Write>(
    writer: &mut W,
    status: &str,
    extra: &[(&str, &str)],
    body: &str,
    head_only: bool,
) -> std::io::Result<()> {
    let mut response = String::new();
    response.push_str("HTTP/1.1 ");
    response.push_str(status);
    response.push_str("\r\nDate: ");
    response.push_str(&http_date(SystemTime::now()));
    response.push_str("\r\nServer: ");
    response.push_str(SERVER);
    response.push_str("\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: ");
    response.push_str(&body.len().to_string());
    response.push_str("\r\nConnection: close\r\n");
    for (name, value) in extra {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    if !head_only {
        response.push_str(body);
    }
    writer.write_all(response.as_bytes())?;
    writer.flush()
}

/// Format `time` as an IMF-fixdate, e.g. `Tue, 29 Feb 2000 00:00:00 GMT`.
fn http_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);

    // Howard Hinnant's `civil_from_days`: days since the epoch to a civil date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    year += u64::from(month <= 2);

    let weekday = (days + 4).rem_euclid(7) as usize;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[weekday],
        day,
        MONTHS[month as usize - 1],
        year,
        hour,
        minute,
        second
    )
}

/// Write one access log line to stdout, tolerating a closed pipe.
fn log(info: &RequestInfo) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(
        stdout,
        "{} {} lang={} status={}",
        sanitize(&info.method),
        sanitize(&info.path),
        sanitize(&info.lang),
        info.status
    );
}

/// Replace control characters and truncate to [`MAX_LOG_FIELD`] characters.
fn sanitize(value: &str) -> String {
    let mut out: String = value
        .chars()
        .take(MAX_LOG_FIELD)
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect();
    if value.chars().nth(MAX_LOG_FIELD).is_some() {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Configuration with neither `--lang` nor `--text`.
    fn default_config() -> Config {
        Config {
            lang: None,
            text: None,
        }
    }

    /// Run one raw request through `handle` and collect the response.
    fn run(request: &str, config: &Config) -> (String, Option<RequestInfo>) {
        let mut reader = BufReader::new(Cursor::new(request.as_bytes().to_vec()));
        let mut writer = Vec::new();
        let info = handle(&mut reader, &mut writer, config).expect("handle must succeed");
        let response = String::from_utf8(writer).expect("response must be utf-8");
        (response, info)
    }

    /// Body following the blank line that ends the header block.
    fn body(response: &str) -> &str {
        let (_, body) = response.split_once("\r\n\r\n").expect("header block");
        body
    }

    /// Value of the named response header.
    fn header_value<'a>(response: &'a str, name: &str) -> Option<&'a str> {
        response.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim())
        })
    }

    #[test]
    fn get_root_defaults_to_english() {
        let (response, info) = run("GET / HTTP/1.1\r\n\r\n", &default_config());
        let info = info.expect("a request must be served");
        assert_eq!(info.status, "200 OK");
        assert_eq!(info.lang, "en");
        assert_eq!(info.path, "/");
        assert_eq!(info.method, "GET");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert_eq!(header_value(&response, "Content-Length"), Some("14"));
        assert_eq!(body(&response), "Hello, World!\n");
        assert!(response.contains("Server: helloworld/"));
        assert!(response.contains("Connection: close\r\n"));
        let date = header_value(&response, "Date").expect("date header");
        assert!(date.ends_with("GMT"), "date was {date}");
        assert_eq!(date.len(), 29);
    }

    #[test]
    fn accept_language_header_is_negotiated() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nAccept-Language: de-DE,de;q=0.9\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").lang, "de");
        assert_eq!(body(&response), "Hallo, Welt!\n");
    }

    #[test]
    fn configured_language_overrides_header() {
        let config = Config {
            lang: Some("nl"),
            text: None,
        };
        let (response, info) = run(
            "GET / HTTP/1.1\r\nAccept-Language: de-DE,de;q=0.9\r\n\r\n",
            &config,
        );
        assert_eq!(info.expect("served").lang, "nl");
        assert_eq!(body(&response), "Hallo, wereld!\n");
    }

    #[test]
    fn configured_text_wins_everywhere() {
        let config = Config {
            lang: Some("nl"),
            text: Some("custom".to_string()),
        };
        let (response, info) = run("GET / HTTP/1.1\r\nAccept-Language: de-DE\r\n\r\n", &config);
        let info = info.expect("served");
        assert_eq!(info.lang, "custom");
        assert_eq!(body(&response), "custom\n");
    }

    #[test]
    fn unknown_path_is_not_found() {
        let (response, info) = run("GET /nope HTTP/1.1\r\n\r\n", &default_config());
        let info = info.expect("served");
        assert_eq!(info.status, "404 Not Found");
        assert_eq!(info.lang, "-");
        assert_eq!(body(&response), "not found\n");
    }

    #[test]
    fn head_has_no_body_but_keeps_length() {
        let (response, info) = run("HEAD / HTTP/1.1\r\n\r\n", &default_config());
        assert_eq!(info.expect("served").status, "200 OK");
        assert_eq!(header_value(&response, "Content-Length"), Some("14"));
        assert_eq!(body(&response), "");
    }

    #[test]
    fn other_methods_are_not_allowed() {
        let (response, info) = run("POST / HTTP/1.1\r\n\r\n", &default_config());
        let info = info.expect("served");
        assert_eq!(info.status, "405 Method Not Allowed");
        assert_eq!(header_value(&response, "Allow"), Some("GET, HEAD"));
        assert_eq!(body(&response), "method not allowed\n");
    }

    #[test]
    fn body_is_drained() {
        let mut reader = BufReader::new(Cursor::new(
            b"GET / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello".to_vec(),
        ));
        let mut writer = Vec::new();
        let info = handle(&mut reader, &mut writer, &default_config())
            .expect("handle must succeed")
            .expect("served");
        assert_eq!(info.status, "200 OK");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).expect("read rest");
        assert!(rest.is_empty(), "leftover bytes: {rest:?}");
    }

    #[test]
    fn invalid_content_length_is_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn conflicting_content_lengths_are_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn oversized_content_length_is_payload_too_large() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nContent-Length: 8388609\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "413 Payload Too Large");
    }

    #[test]
    fn transfer_encoding_is_not_implemented() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "501 Not Implemented");
        assert_eq!(body(&response), "not implemented\n");
    }

    #[test]
    fn long_request_line_is_uri_too_long() {
        let request = format!("{}\r\n", "a".repeat(9000));
        let (_, info) = run(&request, &default_config());
        let info = info.expect("served");
        assert_eq!(info.status, "414 URI Too Long");
        assert_eq!(info.method, "-");
        assert_eq!(info.path, "-");
        assert_eq!(info.lang, "-");
    }

    #[test]
    fn too_many_headers_is_header_fields_too_large() {
        let mut request = String::from("GET / HTTP/1.1\r\n");
        for index in 0..101 {
            request.push_str(&format!("X-{index}: n\r\n"));
        }
        request.push_str("\r\n");
        let (_, info) = run(&request, &default_config());
        assert_eq!(
            info.expect("served").status,
            "431 Request Header Fields Too Large"
        );
    }

    #[test]
    fn malformed_request_lines_are_rejected() {
        let (_, info) = run("GET / HTTP/9.9\r\n\r\n", &default_config());
        assert_eq!(
            info.expect("served").status,
            "505 HTTP Version Not Supported"
        );

        let (_, info) = run("GET / FOO\r\n\r\n", &default_config());
        assert_eq!(info.expect("served").status, "400 Bad Request");

        let (_, info) = run("GET / HTTP/1.1 extra\r\n\r\n", &default_config());
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn empty_input_produces_no_response() {
        let (response, info) = run("", &default_config());
        assert!(info.is_none());
        assert!(response.is_empty());
    }

    #[test]
    fn expect_continue_is_answered_before_body() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "200 OK");
        let final_response = response
            .strip_prefix("HTTP/1.1 100 Continue\r\n\r\n")
            .unwrap_or_else(|| panic!("response was {response:?}"));
        assert!(response.starts_with("HTTP/1.1 100 Continue\r\n\r\n"));
        assert_eq!(body(final_response), "Hello, World!\n");
    }

    #[test]
    fn reject_busy_sends_service_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        reject_busy(server);

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut reader = BufReader::new(client);
        let mut response = String::new();
        reader.read_line(&mut response).expect("read status line");
        assert!(response.starts_with("HTTP/1.1 503"), "was {response:?}");
        reader.read_to_string(&mut response).expect("read body");
        assert!(response.contains("Retry-After: 1"), "was {response:?}");
    }

    #[test]
    fn http_date_formats_known_epochs() {
        assert_eq!(http_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            http_date(UNIX_EPOCH + Duration::from_secs(1_000_000_000)),
            "Sun, 09 Sep 2001 01:46:40 GMT"
        );
        assert_eq!(
            http_date(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "Tue, 29 Feb 2000 00:00:00 GMT"
        );
    }

    #[test]
    fn sanitize_escapes_and_truncates() {
        assert_eq!(sanitize("a\n\u{1b}b"), "a??b");
        let long = "x".repeat(200);
        let sanitized = sanitize(&long);
        assert_eq!(sanitized.len(), MAX_LOG_FIELD + 3);
        assert_eq!(&sanitized[..MAX_LOG_FIELD], &long[..MAX_LOG_FIELD]);
        assert!(sanitized.ends_with("..."));
    }

    #[test]
    fn leading_blank_lines_are_skipped() {
        let (response, info) = run("\r\n\r\nGET / HTTP/1.1\r\n\r\n", &default_config());
        assert_eq!(info.expect("served").status, "200 OK");
        assert_eq!(body(&response), "Hello, World!\n");
    }
}
