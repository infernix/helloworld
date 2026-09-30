//! Dependency-free HTTP/1.x server: one thread per connection.
//!
//! The request path is deliberately small and hardened: every line is length
//! bounded, header and body sizes are capped, malformed input is answered with a
//! status code instead of an error, and the response builder is generic over the
//! writer so the whole layer can be unit tested without a socket.

use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::i18n;
use crate::print_stderr;

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

/// Largest number of connections answered with `503` at once.
const MAX_REJECTS: usize = 64;

/// Stack size for a connection handler thread.
const HANDLER_STACK: usize = 256 * 1024;

/// Read timeout applied to every accepted connection.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Write timeout applied to every accepted connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for reading one complete request (request line, headers and body).
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Total time the read side is drained after a response before closing.
const LINGER_TIMEOUT: Duration = Duration::from_millis(500);

/// Largest number of bytes read while draining a half-closed connection.
const MAX_LINGER_BYTES: usize = 64 * 1024;

/// Pause after a failed `accept` before trying again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Maximum number of characters logged per field.
const MAX_LOG_FIELD: usize = 120;

/// Largest number of queued access log lines before new lines are dropped.
const LOG_QUEUE: usize = 256;

/// `Server` response header value.
const SERVER: &str = concat!("helloworld/", env!("CARGO_PKG_VERSION"));

/// `Vary` value for responses whose body does not depend on the request's language.
const VARY_ENCODING: &str = "Accept-Encoding";

/// `Vary` value for `/` catalog responses, which negotiate `Accept-Language`.
const VARY_LANGUAGE: &str = "Accept-Language, Accept-Encoding";

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
/// At most [`MAX_CONNECTIONS`] connections are served at once, up to
/// [`MAX_REJECTS`] further connections are answered with `503` on their own
/// threads, and any beyond both budgets are closed immediately. Accept errors
/// back off briefly instead of spinning.
pub fn serve(listener: TcpListener, config: Arc<Config>) {
    let budget = Budget::new();
    let logger = Logger::start();
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                // Reserve the slot before spawning so the count can never exceed
                // the cap: two threads racing here cannot both observe a free slot.
                let admission = budget.admit();
                if matches!(admission, Admission::Refuse) {
                    // No capacity left to answer: closing still sheds the load.
                    continue;
                }
                let served = matches!(admission, Admission::Serve);
                // The guard is moved into the closure, so a failed spawn drops it
                // and with it the reservation.
                let slot = Slot {
                    budget: budget.clone(),
                    admission,
                };
                let config = if served {
                    Some(Arc::clone(&config))
                } else {
                    None
                };
                let logger = logger.clone();
                let started = Instant::now();
                let name = if served {
                    "helloworld-conn"
                } else {
                    "helloworld-reject"
                };
                let spawned = thread::Builder::new()
                    .name(name.to_string())
                    .stack_size(HANDLER_STACK)
                    .spawn(move || {
                        let _slot = slot;
                        match config {
                            Some(config) => {
                                if let Err(err) = handle_connection(stream, &config, &logger) {
                                    if !is_disconnect(&err) {
                                        print_stderr(&format!(
                                            "helloworld: connection error: {err}\n"
                                        ));
                                    }
                                }
                            }
                            None => reject_busy(&stream, &logger, started),
                        }
                    });
                if let Err(err) = spawned {
                    print_stderr(&format!("helloworld: failed to spawn handler: {err}\n"));
                }
            }
            Err(err) => {
                print_stderr(&format!("helloworld: accept failed: {err}\n"));
                thread::sleep(ACCEPT_BACKOFF);
            }
        }
    }
}

/// Independent admission budgets for served and rejected connections.
#[derive(Clone)]
struct Budget {
    /// Slots in use by served connections.
    served: Arc<AtomicUsize>,
    /// Slots in use by reject threads.
    rejects: Arc<AtomicUsize>,
}

/// How an accepted connection was admitted.
enum Admission {
    /// Serve the connection on a handler thread.
    Serve,
    /// Answer the connection with `503` on a reject thread.
    Reject,
    /// Close the connection immediately.
    Refuse,
}

impl Budget {
    /// Create a budget with both counters at zero.
    fn new() -> Self {
        Self {
            served: Arc::new(AtomicUsize::new(0)),
            rejects: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Reserve a slot for one connection, or report that none is left.
    ///
    /// The served and reject budgets are independent, so live reject threads never
    /// consume a served slot.
    fn admit(&self) -> Admission {
        let previous = self.served.fetch_add(1, Ordering::Relaxed);
        if previous < MAX_CONNECTIONS {
            return Admission::Serve;
        }
        self.served.fetch_sub(1, Ordering::Relaxed);
        let previous = self.rejects.fetch_add(1, Ordering::Relaxed);
        if previous < MAX_REJECTS {
            return Admission::Reject;
        }
        self.rejects.fetch_sub(1, Ordering::Relaxed);
        Admission::Refuse
    }
}

/// Returns a connection's slot to its budget when the thread ends.
struct Slot {
    /// Budget the slot was taken from.
    budget: Budget,
    /// Which counter the slot holds.
    admission: Admission,
}

impl Drop for Slot {
    fn drop(&mut self) {
        match self.admission {
            Admission::Serve => {
                self.budget.served.fetch_sub(1, Ordering::Relaxed);
            }
            Admission::Reject => {
                self.budget.rejects.fetch_sub(1, Ordering::Relaxed);
            }
            Admission::Refuse => {}
        }
    }
}

/// Answer a connection that arrived while the server was at capacity.
///
/// The response carries no body, so it is also correct for a `HEAD` request, and
/// the rejection is recorded as a `status=503` access-log line.
fn reject_busy(stream: &TcpStream, logger: &Logger, started: Instant) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let peer = stream.peer_addr().ok();
    let info = RequestInfo::new("-", "-", "-", "503 Service Unavailable");
    let mut writer = stream;
    if respond(
        &mut writer,
        info.status,
        VARY_ENCODING,
        &[("Retry-After", "1")],
        "",
        false,
    )
    .is_ok()
    {
        logger.send(log_line(peer, &info, started.elapsed()));
    }
    lingering_close(stream);
}

/// Half-close the write side, then drain the read side until EOF.
///
/// The half-close lets the client see the complete response even while it is still
/// sending a request body, and draining whatever it sends prevents a TCP reset from
/// discarding the response before the client has read it. Draining stops after
/// [`LINGER_TIMEOUT`] in total, or after [`MAX_LINGER_BYTES`] bytes.
fn lingering_close(stream: &TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let deadline = Instant::now() + LINGER_TIMEOUT;
    let mut scratch = [0_u8; 4096];
    let mut drained = 0_usize;
    let mut stream = stream;
    while drained < MAX_LINGER_BYTES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stream.set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        match stream.read(&mut scratch) {
            Ok(0) | Err(_) => break,
            Ok(read) => drained += read,
        }
    }
}

/// Apply the socket timeouts and serve one connection.
fn handle_connection(stream: TcpStream, config: &Config, logger: &Logger) -> std::io::Result<()> {
    let started = Instant::now();
    let peer = stream.peer_addr().ok();
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    // A single descriptor serves the reader, the deadline and the writer, so
    // `MAX_CONNECTIONS + MAX_REJECTS` connections fit inside a default
    // 1024-descriptor limit.
    let mut reader = BufReader::new(&stream);
    let mut deadline = SocketDeadline {
        stream: &stream,
        deadline: Instant::now() + HEAD_TIMEOUT,
    };
    let mut writer = &stream;
    match handle(&mut reader, &mut writer, config, &mut deadline) {
        Ok(Some(info)) => logger.send(log_line(peer, &info, started.elapsed())),
        Ok(None) => {}
        Err(Failure::TimedOut { head_only }) => {
            if let Some(info) = answer_timeout(&mut writer, head_only) {
                logger.send(log_line(peer, &info, started.elapsed()));
            }
        }
        Err(Failure::Io(err)) => return Err(err),
    }
    lingering_close(&stream);
    Ok(())
}

/// Whether `err` is a client that went away rather than a server fault.
///
/// A client that resets or half-closes its side mid-request is normal traffic,
/// so those errors are not worth logging. A write timeout or `WouldBlock` means
/// the client stopped reading, which is also not a server fault.
fn is_disconnect(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
            | ErrorKind::TimedOut
            | ErrorKind::WouldBlock
    )
}

/// How a request ended when it produced no response.
#[derive(Debug)]
enum Failure {
    /// The request head or body did not arrive before the deadline; `head_only`
    /// records whether the client used `HEAD`.
    TimedOut {
        /// Whether the request method was `HEAD`.
        head_only: bool,
    },
    /// The connection failed; a write timeout means the client stopped reading.
    Io(io::Error),
}

impl From<io::Error> for Failure {
    fn from(err: io::Error) -> Self {
        Failure::Io(err)
    }
}

/// Write the `408 Request Timeout` response and describe it.
///
/// Shared by `handle` (deadline expired) and `handle_connection` (socket read
/// timeout) so both paths produce byte-identical output; the request may be
/// unparsed, hence the `-` method/path/lang.
fn answer_timeout<W: Write>(writer: &mut W, head_only: bool) -> Option<RequestInfo> {
    reply(
        writer,
        RequestInfo::new("-", "-", "-", "408 Request Timeout"),
        "request timeout\n",
        &[],
        VARY_ENCODING,
        head_only,
    )
    .ok()
    .flatten()
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

/// Read one line, bounded by `limit` bytes of content.
///
/// A line is accepted iff its content — excluding one trailing `\n` and an optional
/// preceding `\r` — is at most `limit` bytes; a line of exactly `limit` bytes is
/// therefore valid. The deadline is re-armed before every socket read, so a trickled
/// line cannot outlive the request head deadline. Reading stops at the first `\n` (or
/// EOF); invalid UTF-8 is replaced rather than reported, and the caller supplies the
/// scratch buffer so a connection reuses one allocation.
fn read_line<R: BufRead, D: Deadline>(
    reader: &mut R,
    limit: usize,
    buf: &mut Vec<u8>,
    deadline: &mut D,
) -> io::Result<ReadLine> {
    buf.clear();
    loop {
        deadline.arm()?;
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                ReadLine::Eof
            } else {
                finish_line(buf, limit)
            });
        }
        match available.iter().position(|&byte| byte == b'\n') {
            Some(index) => {
                buf.extend_from_slice(&available[..index]);
                reader.consume(index + 1);
                return Ok(finish_line(buf, limit));
            }
            None => {
                let len = available.len();
                buf.extend_from_slice(available);
                reader.consume(len);
                // `limit + 2` covers a maximal content of `limit` bytes plus CRLF.
                if buf.len() >= limit + 2 {
                    return Ok(ReadLine::TooLong);
                }
            }
        }
    }
}

/// Classify a line whose trailing CRLF has already been removed from the input.
///
/// Strips one trailing `\r` and returns [`ReadLine::TooLong`] when more than
/// `limit` content bytes remain.
fn finish_line(buf: &mut Vec<u8>, limit: usize) -> ReadLine {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if buf.len() > limit {
        return ReadLine::TooLong;
    }
    ReadLine::Line(String::from_utf8_lossy(buf).to_string())
}

/// Arms the read deadline before each blocking read of the request.
trait Deadline {
    /// Arm the deadline; fails once it has already passed.
    fn arm(&mut self) -> io::Result<()>;
}

/// No-op deadline, used by tests that drive `handle` without a socket.
impl Deadline for () {
    fn arm(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Deadline that shrinks the socket read timeout to the time left before it expires.
struct SocketDeadline<'a> {
    /// Socket whose read timeout is adjusted.
    stream: &'a TcpStream,
    /// Instant after which the whole request head is late.
    deadline: Instant,
}

impl Deadline for SocketDeadline<'_> {
    fn arm(&mut self) -> io::Result<()> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "request head deadline exceeded",
            ));
        }
        self.stream
            .set_read_timeout(Some(remaining.min(READ_TIMEOUT)))
    }
}

/// Turn a deadline expiry into a [`Failure::TimedOut`]; other errors pass through.
///
/// The `408` itself is written once by the caller (`handle_connection`, or the
/// test harness), so a write that fails after a response has been delivered can
/// never append a second `408`.
fn timeout_or(err: io::Error, head_only: bool) -> Result<Option<RequestInfo>, Failure> {
    match err.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => Err(Failure::TimedOut { head_only }),
        _ => Err(Failure::Io(err)),
    }
}

/// Parse one request and write the response.
///
/// Returns `None` when the peer closed the connection before sending a request
/// line, and otherwise the description of what was served. `deadline` bounds the
/// whole request, so a client that trickles the head or the body is answered with
/// `408` rather than holding a thread.
fn handle<R: BufRead, W: Write, D: Deadline>(
    reader: &mut R,
    writer: &mut W,
    config: &Config,
    deadline: &mut D,
) -> Result<Option<RequestInfo>, Failure> {
    let mut buf = Vec::new();

    // Skip any leading blank lines, then read the request line.
    let request_line = loop {
        match read_line(reader, MAX_REQUEST_LINE, &mut buf, deadline) {
            Err(err) => {
                let head_only = line_method(&buf) == "HEAD";
                return timeout_or(err, head_only);
            }
            Ok(ReadLine::Eof) => return Ok(None),
            Ok(ReadLine::TooLong) => {
                let method = line_method(&buf);
                let head_only = method == "HEAD";
                return fail(
                    writer,
                    &method,
                    "-",
                    "414 URI Too Long",
                    "request line too long\n",
                    VARY_ENCODING,
                    head_only,
                );
            }
            Ok(ReadLine::Line(line)) => {
                if !line.is_empty() {
                    break line;
                }
            }
        }
    };

    let mut parts = request_line.split_whitespace();
    let first = parts.next();
    let head_only = first == Some("HEAD");
    let (method, target, version) = match (first, parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(target), Some(version), None) => (method, target, version),
        _ => {
            return fail(
                writer,
                first.unwrap_or("-"),
                "-",
                "400 Bad Request",
                "bad request\n",
                VARY_ENCODING,
                head_only,
            );
        }
    };

    // Absolute-form targets are accepted (RFC 7230 5.3.2); routing and
    // logging use the origin-form path only.
    let path = request_path(target).to_string();
    // Asterisk-form is only valid for a server-wide OPTIONS request (RFC 7230 5.3.4).
    if target == "*" && method != "OPTIONS" {
        return fail(
            writer,
            method,
            &path,
            "400 Bad Request",
            "bad request\n",
            VARY_ENCODING,
            head_only,
        );
    }
    if !is_token(method) || target.bytes().any(|b| b.is_ascii_control()) {
        return fail(
            writer,
            method,
            &path,
            "400 Bad Request",
            "bad request\n",
            VARY_ENCODING,
            head_only,
        );
    }
    if !version.starts_with("HTTP/") {
        return fail(
            writer,
            method,
            &path,
            "400 Bad Request",
            "bad request\n",
            VARY_ENCODING,
            head_only,
        );
    }
    let Some(version) = parse_version(version) else {
        return fail(
            writer,
            method,
            &path,
            "505 HTTP Version Not Supported",
            "http version not supported\n",
            VARY_ENCODING,
            head_only,
        );
    };

    // Read headers until the blank line that ends the header block.
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut header_bytes = 0_usize;
    loop {
        match read_line(reader, MAX_HEADER_LINE, &mut buf, deadline) {
            Err(err) => return timeout_or(err, head_only),
            Ok(ReadLine::Eof) => break,
            Ok(ReadLine::TooLong) => {
                return fail(
                    writer,
                    method,
                    &path,
                    "431 Request Header Fields Too Large",
                    "headers too large\n",
                    VARY_ENCODING,
                    head_only,
                );
            }
            Ok(ReadLine::Line(line)) => {
                if line.is_empty() {
                    break;
                }
                header_bytes += buf.len() + 2;
                if header_bytes > MAX_HEADER_BYTES || headers.len() >= MAX_HEADERS {
                    return fail(
                        writer,
                        method,
                        &path,
                        "431 Request Header Fields Too Large",
                        "headers too large\n",
                        VARY_ENCODING,
                        head_only,
                    );
                }
                // An obsolete folded line starts with SP or HTAB (RFC 7230
                // 3.2.4); reject it instead of misreading it as a field.
                if line.starts_with([' ', '\t']) {
                    return fail(
                        writer,
                        method,
                        &path,
                        "400 Bad Request",
                        "bad request\n",
                        VARY_ENCODING,
                        head_only,
                    );
                }
                let Some((name, value)) = line.split_once(':') else {
                    return fail(
                        writer,
                        method,
                        &path,
                        "400 Bad Request",
                        "bad request\n",
                        VARY_ENCODING,
                        head_only,
                    );
                };
                // No whitespace is allowed before the colon, and the field name
                // must be a token (RFC 7230 3.2.4, 3.2.6).
                if !is_token(name) {
                    return fail(
                        writer,
                        method,
                        &path,
                        "400 Bad Request",
                        "bad request\n",
                        VARY_ENCODING,
                        head_only,
                    );
                }
                // Field values are VCHAR / obs-text: control characters other
                // than HTAB are malformed.
                if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
                    return fail(
                        writer,
                        method,
                        &path,
                        "400 Bad Request",
                        "bad request\n",
                        VARY_ENCODING,
                        head_only,
                    );
                }
                headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
            }
        }
    }

    // A valid `Host` is required for HTTP/1.1 before any framing decision
    // (RFC 7230 5.4), so a bad one is always a 400.
    if !host_is_valid(version, &headers) {
        return fail(
            writer,
            method,
            &path,
            "400 Bad Request",
            "bad request\n",
            VARY_ENCODING,
            head_only,
        );
    }

    // A body is drained, never buffered, so a chunked request is unsupported.
    if header(&headers, "transfer-encoding").is_some() {
        return fail(
            writer,
            method,
            &path,
            "501 Not Implemented",
            "not implemented\n",
            VARY_ENCODING,
            head_only,
        );
    }

    let mut raw_length: Option<&str> = None;
    for (_, value) in headers.iter().filter(|(name, _)| name == "content-length") {
        // A valid Content-Length is `1*DIGIT`; anything else is malformed.
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return fail(
                writer,
                method,
                &path,
                "400 Bad Request",
                "bad request\n",
                VARY_ENCODING,
                head_only,
            );
        }
        // RFC 7230 3.3.2: conflicting field-values are an unrecoverable
        // framing error, whatever their magnitude, so compare the raw values.
        if let Some(previous) = raw_length {
            if previous != value {
                return fail(
                    writer,
                    method,
                    &path,
                    "400 Bad Request",
                    "bad request\n",
                    VARY_ENCODING,
                    head_only,
                );
            }
        }
        raw_length = Some(value);
    }
    let content_length = match raw_length {
        // Digits-only, so only an overflowing magnitude can fail to parse.
        Some(value) => match value.parse::<u64>() {
            Ok(parsed) => parsed,
            Err(_) => {
                return fail(
                    writer,
                    method,
                    &path,
                    "413 Payload Too Large",
                    "payload too large\n",
                    VARY_ENCODING,
                    head_only,
                );
            }
        },
        None => 0,
    };
    if content_length > MAX_BODY as u64 {
        return fail(
            writer,
            method,
            &path,
            "413 Payload Too Large",
            "payload too large\n",
            VARY_ENCODING,
            head_only,
        );
    }

    // The response carries no content coding, so a client that refuses
    // `identity` has no acceptable representation (RFC 9110 12.5.3). Reject
    // before the body is invited with `100 Continue`.
    if !identity_is_acceptable(&headers) {
        return fail(
            writer,
            method,
            &path,
            "406 Not Acceptable",
            "not acceptable\n",
            VARY_ENCODING,
            head_only,
        );
    }

    match header(&headers, "expect") {
        // An HTTP/1.0 client cannot understand 1xx; ignore the expectation
        // (RFC 7231 5.1.1) and read the body as usual.
        Some(value) if version == Version::Http11 && value.eq_ignore_ascii_case("100-continue") => {
            writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            writer.flush()?;
        }
        Some(value) if value.eq_ignore_ascii_case("100-continue") => {}
        Some(_) => {
            return fail(
                writer,
                method,
                &path,
                "417 Expectation Failed",
                "expectation failed\n",
                VARY_ENCODING,
                head_only,
            );
        }
        None => {}
    }

    // Drain the body, re-arming the deadline before every chunk so a client
    // that trickles the body cannot hold the connection open.
    let mut remaining = content_length;
    let mut scratch = [0_u8; 8192];
    while remaining > 0 {
        if let Err(err) = deadline.arm() {
            return timeout_or(err, head_only);
        }
        let read = match (&mut *reader).take(remaining).read(&mut scratch) {
            Ok(read) => read,
            Err(err) => return timeout_or(err, head_only),
        };
        if read == 0 {
            break;
        }
        remaining -= read as u64;
    }
    if remaining != 0 {
        return fail(
            writer,
            method,
            &path,
            "400 Bad Request",
            "bad request\n",
            VARY_ENCODING,
            head_only,
        );
    }

    let (status, lang, body, extra, vary): (
        &'static str,
        String,
        String,
        Vec<(&'static str, &'static str)>,
        &'static str,
    ) = match method {
        "GET" | "HEAD" => match path.as_str() {
            "/" => greeting(config, &headers),
            "/healthz" => (
                "200 OK",
                "-".to_string(),
                "ok\n".to_string(),
                Vec::new(),
                VARY_ENCODING,
            ),
            _ => (
                "404 Not Found",
                "-".to_string(),
                "not found\n".to_string(),
                Vec::new(),
                VARY_ENCODING,
            ),
        },
        _ => (
            "405 Method Not Allowed",
            "-".to_string(),
            "method not allowed\n".to_string(),
            vec![("Allow", "GET, HEAD")],
            VARY_ENCODING,
        ),
    };

    reply(
        writer,
        RequestInfo::new(method, &path, &lang, status),
        &body,
        &extra,
        vary,
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

    /// The three-digit status code, for the access log.
    fn code(&self) -> &str {
        self.status.get(..3).unwrap_or(self.status)
    }
}

/// Write a response and describe it as a [`RequestInfo`].
fn reply<W: Write>(
    writer: &mut W,
    info: RequestInfo,
    body: &str,
    extra: &[(&str, &str)],
    vary: &str,
    head_only: bool,
) -> Result<Option<RequestInfo>, Failure> {
    respond(writer, info.status, vary, extra, body, head_only)?;
    Ok(Some(info))
}

/// Reject a request before routing, logging `-` as its language.
///
/// `vary` is written as the response's `Vary` value, as in [`reply`].
fn fail<W: Write>(
    writer: &mut W,
    method: &str,
    path: &str,
    status: &'static str,
    body: &str,
    vary: &str,
    head_only: bool,
) -> Result<Option<RequestInfo>, Failure> {
    reply(
        writer,
        RequestInfo::new(method, path, "-", status),
        body,
        &[],
        vary,
        head_only,
    )
}

/// Resolve the greeting, response headers and `Vary` value for a request.
///
/// Follows the documented precedence: `--text`, then `--lang`, then
/// `Accept-Language`, then the default. `Content-Language` reflects the language
/// actually served, while `Vary` names the request headers the body depends on:
/// catalog responses vary on `Accept-Language` and `Accept-Encoding`, and
/// `--lang`/`--text` responses on `Accept-Encoding` alone. `--text` serves a
/// fixed string, so it advertises no `Content-Language`. A header that leaves no
/// catalog language acceptable is answered with `406 Not Acceptable`.
fn greeting(
    config: &Config,
    headers: &[(String, String)],
) -> (
    &'static str,
    String,
    String,
    Vec<(&'static str, &'static str)>,
    &'static str,
) {
    if let Some(text) = &config.text {
        let mut body = text.clone();
        body.push('\n');
        return (
            "200 OK",
            "custom".to_string(),
            body,
            Vec::new(),
            VARY_ENCODING,
        );
    }
    if let Some(code) = config.lang {
        return (
            "200 OK",
            code.to_string(),
            format!("{}\n", i18n::greeting_for(code)),
            vec![("Content-Language", code)],
            VARY_ENCODING,
        );
    }
    let combined = combined_header(headers, "accept-language");
    match i18n::negotiate(combined.as_deref().unwrap_or("")) {
        i18n::Negotiated::Language(code) => (
            "200 OK",
            code.to_string(),
            format!("{}\n", i18n::greeting_for(code)),
            vec![("Content-Language", code)],
            VARY_LANGUAGE,
        ),
        i18n::Negotiated::NotAcceptable => (
            "406 Not Acceptable",
            "-".to_string(),
            "not acceptable\n".to_string(),
            Vec::new(),
            VARY_LANGUAGE,
        ),
    }
}

/// First value of the named (lowercase) header, if present.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Join every value of the named (lowercase) header in order with `", "`.
///
/// Multiple field lines with the same name are equivalent to one comma-separated
/// list (RFC 9110 5.3), so callers that need the whole list use this instead of
/// [`header`]. Returns `None` when the header is absent.
fn combined_header(headers: &[(String, String)], name: &str) -> Option<String> {
    let mut joined: Option<String> = None;
    for (key, value) in headers {
        if key == name {
            match &mut joined {
                Some(existing) => {
                    existing.push_str(", ");
                    existing.push_str(value);
                }
                None => joined = Some(value.clone()),
            }
        }
    }
    joined
}

/// Whether `name` is a valid HTTP `token` (RFC 7230 3.2.6).
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Strip an absolute-form request target down to its origin-form path.
///
/// A server must accept absolute-form targets (RFC 7230 5.3.2); the scheme and
/// authority are ignored, and the path is routed and logged as usual.
fn origin_path(target: &str) -> &str {
    let rest = ["http://", "https://"].iter().find_map(|scheme| {
        target
            .get(..scheme.len())
            .filter(|head| head.eq_ignore_ascii_case(scheme))
            .map(|_| &target[scheme.len()..])
    });
    match rest {
        Some(rest) => match rest.find(['/', '?', '#']) {
            Some(index) => &rest[index..],
            None => "/",
        },
        None => target,
    }
}

/// Extract the origin-form path a request target routes on.
///
/// Absolute-form targets are reduced to their origin form (RFC 7230 5.3.2), the
/// query and fragment are dropped, and a target with an empty path (`?q=1`,
/// `http://host?q=1`, `http://host#f`) routes as `/`, since an empty path is
/// equivalent to `/` (RFC 3986 6.2.3).
fn request_path(target: &str) -> &str {
    let path = origin_path(target).split(['?', '#']).next().unwrap_or("");
    if path.is_empty() {
        "/"
    } else {
        path
    }
}

/// HTTP version of a request, after folding higher `1.x` minors to 1.1.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Version {
    /// HTTP/1.0.
    Http10,
    /// HTTP/1.1, or any higher `HTTP/1.x` minor (RFC 7230 2.6).
    Http11,
}

/// Parse a request's HTTP version token.
///
/// A higher `HTTP/1.x` minor version is processed as 1.1 (RFC 7230 2.6); a
/// different major version is unsupported.
fn parse_version(version: &str) -> Option<Version> {
    match version {
        "HTTP/1.0" => Some(Version::Http10),
        _ => {
            let minor = version.strip_prefix("HTTP/1.")?;
            if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(Version::Http11)
        }
    }
}

/// Method token of a possibly partial request line, for logging and `HEAD` detection.
fn line_method(buf: &[u8]) -> String {
    String::from_utf8_lossy(buf)
        .split_whitespace()
        .next()
        .unwrap_or("-")
        .to_string()
}

/// Whether the `Host` header(s) satisfy the request's HTTP version.
///
/// HTTP/1.1 requires exactly one non-empty `Host` whose value is a valid
/// `uri-host [ ":" port ]` (RFC 9110 7.2); HTTP/1.0 makes the field optional,
/// but a value that is present must still be a valid authority. Duplicates are
/// rejected for both versions.
fn host_is_valid(version: Version, headers: &[(String, String)]) -> bool {
    let mut host = None;
    let mut count = 0_usize;
    for (_, value) in headers.iter().filter(|(name, _)| name == "host") {
        count += 1;
        host = Some(value.as_str());
    }
    if count > 1 {
        return false;
    }
    match host {
        None => version == Version::Http10,
        Some(value) => !value.is_empty() && authority_is_valid(value),
    }
}

/// Whether `value` is a `uri-host [ ":" port ]` (RFC 9110 7.2).
///
/// The bracketed form is an IP-literal: `[`, a non-empty run of URI bytes
/// (letters, digits, `:`, and the unreserved and sub-delimiter punctuation), `]`,
/// and an optional `:` followed by digits. Otherwise the value is a `reg-name`
/// with an optional trailing `:port`, where `port` is `*DIGIT` so an empty port
/// is allowed; a `:` anywhere else in the host makes the value invalid.
fn authority_is_valid(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            return false;
        };
        let literal = &rest[..close];
        if literal.is_empty()
            || !literal
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b":.%_-~!$&'()*+,;=".contains(&b))
        {
            return false;
        }
        let tail = &rest[close + 1..];
        return tail.is_empty()
            || tail
                .strip_prefix(':')
                .is_some_and(|port| port.bytes().all(|b| b.is_ascii_digit()));
    }
    let host = match value.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => value,
    };
    !host.contains(':') && reg_name_is_valid(host)
}

/// Whether `host` is a `reg-name` (RFC 3986 3.2.2).
///
/// Every byte is an unreserved or sub-delimiter ASCII character, and every `%`
/// introduces a two-digit hexadecimal percent-escape.
fn reg_name_is_valid(host: &str) -> bool {
    let bytes = host.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
            continue;
        }
        if !(byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=".contains(&byte)) {
            return false;
        }
        index += 1;
    }
    true
}

/// Whether the request accepts the `identity` content coding.
///
/// The response carries no content coding, so `identity` is acceptable by default
/// and is refused only when the `Accept-Encoding` list rules it out (RFC 9110
/// 12.5.3): an `identity` entry with weight zero, or a `*` entry with weight zero
/// and no `identity` entry at all. The coding list is parsed by
/// [`i18n::parse_weighted_list`], so a malformed weight drops its entry and a
/// later entry for the same coding overrides an earlier one. An absent or empty
/// header accepts `identity`.
fn identity_is_acceptable(headers: &[(String, String)]) -> bool {
    let Some(combined) = combined_header(headers, "accept-encoding") else {
        return true;
    };
    let mut identity: Option<f32> = None;
    let mut wildcard: Option<f32> = None;
    for (weight, coding) in i18n::parse_weighted_list(&combined) {
        if coding.eq_ignore_ascii_case("identity") {
            identity = Some(weight);
        } else if coding == "*" {
            wildcard = Some(weight);
        }
    }
    match identity.or(wildcard) {
        Some(weight) => weight > 0.0,
        None => true,
    }
}

/// Serialize one response: status line, fixed headers, `Vary`, extras, then the body.
///
/// `vary` is written as the single `Vary` field of every final response, so the
/// cache key always covers the request headers the response was derived from.
fn respond<W: Write>(
    writer: &mut W,
    status: &str,
    vary: &str,
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
    response.push_str("\r\nConnection: close\r\nVary: ");
    response.push_str(vary);
    response.push_str("\r\n");
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

/// Split Unix seconds into `(year, month, day, hour, minute, second)` UTC.
///
/// Uses Howard Hinnant's `civil_from_days`; shared by [`http_date`] and
/// [`iso8601`] so the two renderings cannot drift.
fn civil(secs: u64) -> (u64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);

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

    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    (
        year,
        month as u32,
        day as u32,
        hour as u32,
        minute as u32,
        second as u32,
    )
}

/// Format `time` as an IMF-fixdate, e.g. `Tue, 29 Feb 2000 00:00:00 GMT`.
fn http_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil(secs);
    let weekday = (secs.div_euclid(86_400) + 4).rem_euclid(7) as usize;
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

/// Format `time` as an ISO 8601 UTC timestamp, e.g. `2026-09-30T11:15:00Z`.
fn iso8601(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Bounded access-log sink: handlers enqueue, one writer thread drains to stdout.
#[derive(Clone)]
struct Logger {
    /// Queue shared with the writer thread.
    sender: mpsc::SyncSender<String>,
}

impl Logger {
    /// Start the writer thread and return a handle to it.
    ///
    /// The thread locks stdout once and writes every received line until the queue
    /// closes or a write fails, so a blocked stdout can never stall a connection.
    fn start() -> Logger {
        let (sender, receiver) = mpsc::sync_channel::<String>(LOG_QUEUE);
        let spawned = thread::Builder::new()
            .name("helloworld-log".to_string())
            .spawn(move || {
                let mut stdout = io::stdout().lock();
                for line in receiver {
                    if writeln!(stdout, "{line}").is_err() {
                        break;
                    }
                }
            });
        if let Err(err) = spawned {
            print_stderr(&format!("helloworld: failed to spawn log writer: {err}\n"));
        }
        Logger { sender }
    }

    /// Enqueue one line, dropping it when the queue is full or the writer is gone.
    fn send(&self, line: String) {
        let _ = self.sender.try_send(line);
    }

    /// A logger that hands lines to the caller instead of a thread.
    #[cfg(test)]
    fn capturing() -> (Logger, mpsc::Receiver<String>) {
        let (sender, receiver) = mpsc::sync_channel(LOG_QUEUE);
        (Logger { sender }, receiver)
    }
}

/// Format one access log line.
fn log_line(peer: Option<SocketAddr>, info: &RequestInfo, elapsed: Duration) -> String {
    let peer = peer
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{} {} {} {} lang={} status={} dur={:.1}ms",
        iso8601(SystemTime::now()),
        peer,
        sanitize(&info.method),
        sanitize(&info.path),
        sanitize(&info.lang),
        info.code(),
        elapsed.as_secs_f64() * 1000.0,
    )
}

/// Replace control characters and truncate a field longer than [`MAX_LOG_FIELD`].
///
/// A field of at most [`MAX_LOG_FIELD`] characters is logged unchanged (apart from
/// control characters becoming `?`); a longer one becomes its first
/// `MAX_LOG_FIELD - 3` characters followed by `...`.
fn sanitize(value: &str) -> String {
    let escaped = || {
        value
            .chars()
            .map(|ch| if ch.is_control() { '?' } else { ch })
    };
    if value.chars().count() > MAX_LOG_FIELD {
        let mut out: String = escaped().take(MAX_LOG_FIELD - 3).collect();
        out.push_str("...");
        out
    } else {
        escaped().collect()
    }
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
        let info = handle(&mut reader, &mut writer, config, &mut ()).expect("handle must succeed");
        let response = String::from_utf8(writer).expect("response must be utf-8");
        (response, info)
    }

    /// Run one request through `handle` with a deadline, mirroring
    /// `handle_connection`: a deadline expiry is answered with `408`.
    fn run_deadline<R: BufRead, D: Deadline>(
        reader: &mut R,
        config: &Config,
        deadline: &mut D,
    ) -> (String, Option<RequestInfo>) {
        let mut writer = Vec::new();
        let info = match handle(reader, &mut writer, config, deadline) {
            Ok(info) => info,
            Err(Failure::TimedOut { head_only }) => answer_timeout(&mut writer, head_only),
            Err(Failure::Io(err)) => panic!("io error: {err}"),
        };
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

    /// One `accept-encoding` request header carrying `value`.
    fn accept_encoding(value: &str) -> Vec<(String, String)> {
        vec![("accept-encoding".to_string(), value.to_string())]
    }

    /// Deadline that is already expired, forcing the `408` path.
    struct ExpiredDeadline;

    impl Deadline for ExpiredDeadline {
        fn arm(&mut self) -> io::Result<()> {
            Err(io::Error::new(ErrorKind::TimedOut, "test"))
        }
    }

    #[test]
    fn get_root_defaults_to_english() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
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
            "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: de-DE,de;q=0.9\r\n\r\n",
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
            "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: de-DE,de;q=0.9\r\n\r\n",
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
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: de-DE\r\n\r\n",
            &config,
        );
        let info = info.expect("served");
        assert_eq!(info.lang, "custom");
        assert_eq!(body(&response), "custom\n");
    }

    #[test]
    fn unknown_path_is_not_found() {
        let (response, info) = run(
            "GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        let info = info.expect("served");
        assert_eq!(info.status, "404 Not Found");
        assert_eq!(info.lang, "-");
        assert_eq!(body(&response), "not found\n");
    }

    #[test]
    fn head_has_no_body_but_keeps_length() {
        let (response, info) = run(
            "HEAD / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "200 OK");
        assert_eq!(header_value(&response, "Content-Length"), Some("14"));
        assert_eq!(body(&response), "");
    }

    #[test]
    fn other_methods_are_not_allowed() {
        let (response, info) = run(
            "POST / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        let info = info.expect("served");
        assert_eq!(info.status, "405 Method Not Allowed");
        assert_eq!(header_value(&response, "Allow"), Some("GET, HEAD"));
        assert_eq!(body(&response), "method not allowed\n");
    }

    #[test]
    fn body_is_drained() {
        let mut reader = BufReader::new(Cursor::new(
            b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello".to_vec(),
        ));
        let mut writer = Vec::new();
        let info = handle(&mut reader, &mut writer, &default_config(), &mut ())
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
            "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: x\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn signed_content_length_is_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: +5\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn conflicting_content_lengths_are_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn oversized_content_length_is_payload_too_large() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8388609\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "413 Payload Too Large");
    }

    #[test]
    fn missing_host_beats_payload_too_large() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nContent-Length: 8388609\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn missing_host_beats_not_implemented() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn overflowing_content_length_is_payload_too_large() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 99999999999999999999\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "413 Payload Too Large");
    }

    #[test]
    fn transfer_encoding_is_not_implemented() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "501 Not Implemented");
        assert_eq!(body(&response), "not implemented\n");
    }

    #[test]
    fn http11_without_host_is_bad_request() {
        let (_, info) = run("GET / HTTP/1.1\r\n\r\n", &default_config());
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn duplicate_host_headers_are_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn empty_host_is_bad_request() {
        let (_, info) = run("GET / HTTP/1.1\r\nHost: \r\n\r\n", &default_config());
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn http10_without_host_is_served() {
        let (response, info) = run("GET / HTTP/1.0\r\n\r\n", &default_config());
        assert_eq!(info.expect("served").status, "200 OK");
        assert_eq!(body(&response), "Hello, World!\n");
    }

    #[test]
    fn authority_is_valid_accepts_valid_hosts() {
        for host in [
            "example.com",
            "example.com:8080",
            "127.0.0.1",
            "127.0.0.1:21212",
            "[::1]",
            "[::1]:21212",
            "[2001:db8::1]",
            "[fe80::1%25eth0]",
            "[fe80::1%eth0]",
            "xn--nqv7f.example",
            "EXAMPLE.COM.",
            "a,b",
            "_dmarc.example.com",
            ":21212",
            "example.com:",
            "%41.example",
            "localhost",
        ] {
            assert!(authority_is_valid(host), "should accept {host:?}");
        }
    }

    #[test]
    fn authority_is_valid_rejects_invalid_hosts() {
        for host in [
            "user@host",
            "[::1",
            "[]",
            "[a/b]",
            "a:1:2",
            "%zz",
            "%4",
            "a@b:80",
            "[::1]:x",
            "[::1]x",
        ] {
            assert!(!authority_is_valid(host), "should reject {host:?}");
        }
    }

    #[test]
    fn host_with_userinfo_is_bad_request() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: user@host\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
        assert_eq!(body(&response), "bad request\n");
    }

    #[test]
    fn host_with_unterminated_bracket_is_bad_request() {
        let (response, info) = run("GET / HTTP/1.1\r\nHost: [::1\r\n\r\n", &default_config());
        assert_eq!(info.expect("served").status, "400 Bad Request");
        assert_eq!(body(&response), "bad request\n");
    }

    #[test]
    fn unexpected_expectation_is_417() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nExpect: nope\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "417 Expectation Failed");
        assert_eq!(body(&response), "expectation failed\n");
    }

    #[test]
    fn long_request_line_is_uri_too_long() {
        let request = format!("GET /{}\r\n", "a".repeat(9000));
        let (_, info) = run(&request, &default_config());
        let info = info.expect("served");
        assert_eq!(info.status, "414 URI Too Long");
        assert_eq!(info.method, "GET");
        assert_eq!(info.path, "-");
        assert_eq!(info.lang, "-");
    }

    #[test]
    fn request_line_at_limit_is_served() {
        let padding = " ".repeat(MAX_REQUEST_LINE - "GET / HTTP/1.1".len());
        let request = format!("GET / HTTP/1.1{padding}\r\nHost: localhost\r\n\r\n");
        assert_eq!(request.find("\r\n").expect("line"), MAX_REQUEST_LINE);
        let (_, info) = run(&request, &default_config());
        assert_eq!(info.expect("served").status, "200 OK");
    }

    #[test]
    fn request_line_over_limit_is_uri_too_long() {
        let padding = " ".repeat(MAX_REQUEST_LINE - "GET / HTTP/1.1".len() + 1);
        let request = format!("GET / HTTP/1.1{padding}\r\nHost: localhost\r\n\r\n");
        assert_eq!(request.find("\r\n").expect("line"), MAX_REQUEST_LINE + 1);
        let (_, info) = run(&request, &default_config());
        assert_eq!(info.expect("served").status, "414 URI Too Long");
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
            "GET / HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello",
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
    fn expired_deadline_is_request_timeout() {
        let mut reader = BufReader::new(Cursor::new(
            b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
        ));
        let (response, info) = run_deadline(&mut reader, &default_config(), &mut ExpiredDeadline);
        let info = info.expect("served");
        assert_eq!(info.status, "408 Request Timeout");
        assert_eq!(info.method, "-");
        assert_eq!(info.path, "-");
        assert_eq!(info.lang, "-");
        assert!(response.starts_with("HTTP/1.1 408 Request Timeout\r\n"));
        assert_eq!(body(&response), "request timeout\n");
    }

    #[test]
    fn answer_timeout_writes_408() {
        let mut writer = Vec::new();
        let info = answer_timeout(&mut writer, false).expect("served");
        assert_eq!(info.status, "408 Request Timeout");
        let response = String::from_utf8(writer).expect("response must be utf-8");
        assert!(response.starts_with("HTTP/1.1 408 Request Timeout\r\n"));
        assert_eq!(body(&response), "request timeout\n");
        assert_eq!(header_value(&response, "Vary"), Some("Accept-Encoding"));
    }

    #[test]
    fn combined_accept_language_lines_are_joined_in_order() {
        let headers = vec![
            ("accept-language".to_string(), "de;q=0.1".to_string()),
            ("accept-language".to_string(), "ja;q=1".to_string()),
        ];
        assert_eq!(
            combined_header(&headers, "accept-language").as_deref(),
            Some("de;q=0.1, ja;q=1")
        );
        assert_eq!(combined_header(&headers, "accept"), None);

        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: de;q=0.1\r\nAccept-Language: ja;q=1\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").lang, "ja");
        assert_eq!(header_value(&response, "Content-Language"), Some("ja"));
    }

    #[test]
    fn unacceptable_header_is_406_with_vary() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: en;q=0\r\n\r\n",
            &default_config(),
        );
        let info = info.expect("served");
        assert_eq!(info.status, "406 Not Acceptable");
        assert_eq!(info.lang, "-");
        assert_eq!(body(&response), "not acceptable\n");
        assert_eq!(
            header_value(&response, "Vary"),
            Some("Accept-Language, Accept-Encoding")
        );
        assert_eq!(header_value(&response, "Content-Language"), None);
    }

    #[test]
    fn acceptable_header_is_200_with_language_and_vary() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Language: de\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "200 OK");
        assert_eq!(header_value(&response, "Content-Language"), Some("de"));
        assert_eq!(
            header_value(&response, "Vary"),
            Some("Accept-Language, Accept-Encoding")
        );
    }

    #[test]
    fn configured_language_advertises_content_language_and_encoding() {
        let config = Config {
            lang: Some("nl"),
            text: None,
        };
        let (response, _) = run("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n", &config);
        assert_eq!(header_value(&response, "Content-Language"), Some("nl"));
        assert_eq!(header_value(&response, "Vary"), Some("Accept-Encoding"));
    }

    #[test]
    fn custom_text_advertises_no_language_headers() {
        let config = Config {
            lang: None,
            text: Some("hi".to_string()),
        };
        let (response, _) = run("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n", &config);
        assert_eq!(header_value(&response, "Content-Language"), None);
        assert_eq!(header_value(&response, "Vary"), Some("Accept-Encoding"));
    }

    #[test]
    fn other_routes_advertise_no_language_headers() {
        let (health, _) = run(
            "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(header_value(&health, "Content-Language"), None);
        assert_eq!(header_value(&health, "Vary"), Some("Accept-Encoding"));

        let (missing, _) = run(
            "GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(header_value(&missing, "Content-Language"), None);
        assert_eq!(header_value(&missing, "Vary"), Some("Accept-Encoding"));
    }

    #[test]
    fn identity_is_acceptable_honours_the_coding_list() {
        assert!(identity_is_acceptable(&[]));
        assert!(identity_is_acceptable(&accept_encoding("")));
        assert!(identity_is_acceptable(&accept_encoding("gzip")));
        assert!(identity_is_acceptable(&accept_encoding("gzip;q=0")));
        assert!(identity_is_acceptable(&accept_encoding("identity")));
        assert!(identity_is_acceptable(&accept_encoding("identity;q=1")));
        assert!(identity_is_acceptable(&accept_encoding("*;q=0, identity")));
        assert!(identity_is_acceptable(&accept_encoding(
            "identity;q=0, identity;q=1"
        )));
        assert!(identity_is_acceptable(&accept_encoding("identity;q=bogus")));

        assert!(!identity_is_acceptable(&accept_encoding("identity;q=0")));
        assert!(!identity_is_acceptable(&accept_encoding("*;q=0")));
        assert!(!identity_is_acceptable(&accept_encoding("gzip, *;q=0")));
        assert!(!identity_is_acceptable(&accept_encoding(
            "identity;q=0, *;q=1"
        )));
    }

    #[test]
    fn identity_refusal_is_406() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: identity;q=0\r\n\r\n",
            &default_config(),
        );
        let info = info.expect("served");
        assert_eq!(info.status, "406 Not Acceptable");
        assert_eq!(info.lang, "-");
        assert_eq!(body(&response), "not acceptable\n");
        assert_eq!(header_value(&response, "Vary"), Some("Accept-Encoding"));
        assert_eq!(header_value(&response, "Content-Language"), None);
    }

    #[test]
    fn encoding_that_accepts_identity_is_200() {
        let (response, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "200 OK");
        assert_eq!(body(&response), "Hello, World!\n");
        assert_eq!(
            header_value(&response, "Vary"),
            Some("Accept-Language, Accept-Encoding")
        );
    }

    #[test]
    fn budget_serves_up_to_the_connection_cap() {
        let budget = Budget::new();
        for _ in 0..MAX_CONNECTIONS {
            assert!(matches!(budget.admit(), Admission::Serve));
        }
        assert!(matches!(budget.admit(), Admission::Reject));
    }

    #[test]
    fn budget_rejects_are_capped() {
        let budget = Budget::new();
        for _ in 0..MAX_CONNECTIONS {
            assert!(matches!(budget.admit(), Admission::Serve));
        }
        for _ in 0..MAX_REJECTS {
            assert!(matches!(budget.admit(), Admission::Reject));
        }
        assert!(matches!(budget.admit(), Admission::Refuse));
    }

    #[test]
    fn budget_keeps_rejects_from_consuming_served_slots() {
        let budget = Budget::new();
        for _ in 0..MAX_CONNECTIONS - 1 {
            assert!(matches!(budget.admit(), Admission::Serve));
        }
        // Live reject threads must not shrink the served budget.
        budget.rejects.store(MAX_REJECTS, Ordering::Relaxed);
        assert!(matches!(budget.admit(), Admission::Serve));
    }

    #[test]
    fn slot_returns_its_counter_on_drop() {
        let budget = Budget::new();
        let slot = Slot {
            budget: budget.clone(),
            admission: budget.admit(),
        };
        assert_eq!(budget.served.load(Ordering::Relaxed), 1);
        drop(slot);
        assert_eq!(budget.served.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reject_busy_sends_service_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let (logger, lines) = Logger::capturing();
        reject_busy(&server, &logger, Instant::now());

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut reader = BufReader::new(client);
        let mut response = String::new();
        reader.read_line(&mut response).expect("read status line");
        assert!(response.starts_with("HTTP/1.1 503"), "was {response:?}");
        reader.read_to_string(&mut response).expect("read body");
        assert!(response.contains("Retry-After: 1"), "was {response:?}");
        assert!(
            response.contains("Vary: Accept-Encoding"),
            "was {response:?}"
        );
        assert_eq!(body(&response), "", "was {response:?}");

        let line = lines
            .recv_timeout(Duration::from_secs(1))
            .expect("log line");
        assert!(line.contains("status=503"), "was {line}");
    }

    #[test]
    fn response_is_complete_and_closed_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let handler = thread::spawn(move || {
            let (logger, _lines) = Logger::capturing();
            handle_connection(server, &default_config(), &logger)
        });

        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello")
            .expect("write request");
        let _ = client.shutdown(Shutdown::Write);
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut reader = BufReader::new(client);
        let mut response = String::new();
        reader.read_to_string(&mut response).expect("read response");
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "was {response:?}"
        );
        assert!(response.ends_with("Hello, World!\n"), "was {response:?}");
        handler.join().expect("handler").expect("served");
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
    fn iso8601_formats_known_instants() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            iso8601(UNIX_EPOCH + Duration::from_secs(1_000_000_000)),
            "2001-09-09T01:46:40Z"
        );
        assert_eq!(
            iso8601(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn sanitize_escapes_and_truncates() {
        assert_eq!(sanitize("a\n\u{1b}b"), "a??b");
        let exact = "x".repeat(MAX_LOG_FIELD);
        assert_eq!(sanitize(&exact), exact);
        let over = "x".repeat(MAX_LOG_FIELD + 1);
        let sanitized = sanitize(&over);
        assert_eq!(sanitized.len(), MAX_LOG_FIELD);
        assert_eq!(&sanitized[..MAX_LOG_FIELD - 3], &over[..MAX_LOG_FIELD - 3]);
        assert!(sanitized.ends_with("..."));
    }

    #[test]
    fn leading_blank_lines_are_skipped() {
        let (response, info) = run(
            "\r\n\r\nGET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "200 OK");
        assert_eq!(body(&response), "Hello, World!\n");
    }

    /// Deadline that expires after a fixed number of `arm` calls.
    struct CountdownDeadline {
        /// Arms left before the deadline expires.
        arms: usize,
    }

    impl Deadline for CountdownDeadline {
        fn arm(&mut self) -> io::Result<()> {
            if self.arms == 0 {
                return Err(io::Error::new(ErrorKind::TimedOut, "test"));
            }
            self.arms -= 1;
            Ok(())
        }
    }

    /// Reader that hands out one byte per read, like a client trickling a body.
    struct Trickle {
        /// Bytes still to hand out.
        data: Vec<u8>,
        /// Offset of the next byte.
        pos: usize,
    }

    impl Trickle {
        /// Build a reader over `data`.
        fn new(data: Vec<u8>) -> Self {
            Self { data, pos: 0 }
        }
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    impl BufRead for Trickle {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Ok(&self.data[self.pos.min(self.data.len())..(self.pos + 1).min(self.data.len())])
        }

        fn consume(&mut self, amount: usize) {
            self.pos = (self.pos + amount).min(self.data.len());
        }
    }

    #[test]
    fn slow_body_is_request_timeout() {
        let body = "x".repeat(100);
        let request =
            format!("GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n{body}");
        // The head now consumes one arm per trickled byte, so the budget must
        // outlast the whole head and still expire inside the body drain.
        let head = request.find("\r\n\r\n").expect("head") + 4;
        let mut reader = BufReader::new(Trickle::new(request.into_bytes()));
        let (_, info) = run_deadline(
            &mut reader,
            &default_config(),
            &mut CountdownDeadline { arms: head + 6 },
        );
        assert_eq!(info.expect("served").status, "408 Request Timeout");
    }

    #[test]
    fn trickled_head_is_request_timeout() {
        let request = "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut reader = BufReader::new(Trickle::new(request.as_bytes().to_vec()));
        let (_, info) = run_deadline(
            &mut reader,
            &default_config(),
            &mut CountdownDeadline { arms: 4 },
        );
        assert_eq!(info.expect("served").status, "408 Request Timeout");
    }

    #[test]
    fn a_full_log_queue_drops_lines_instead_of_blocking() {
        let (logger, lines) = Logger::capturing();
        for index in 0..LOG_QUEUE * 2 {
            logger.send(format!("line {index}"));
        }
        drop(logger);
        assert_eq!(lines.iter().count(), LOG_QUEUE);
    }

    #[test]
    fn lingering_close_gives_up_after_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let (done, finished) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            lingering_close(&server);
            done.send(()).expect("send");
        });
        // Keep the read side fed so only the deadline can stop the drain.
        let started = Instant::now();
        let mut closed = false;
        while started.elapsed() < Duration::from_secs(2) {
            if finished.try_recv().is_ok() {
                closed = true;
                break;
            }
            if client.write_all(b"x").is_err() {
                // The drain gave up and closed the socket.
                closed = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(closed, "lingering_close drained past LINGER_TIMEOUT");
        handle.join().expect("join");
    }

    #[test]
    fn whitespace_before_colon_is_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost : localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn obs_fold_is_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nX-A: 1\r\n folded\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn control_character_in_header_value_is_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nX-A: a\u{1}b\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn non_token_header_name_is_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nX A: 1\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn header_line_without_colon_is_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nnope\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn http10_ignores_expect_100_continue() {
        let (response, info) = run(
            "GET / HTTP/1.0\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\nabc",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "200 OK");
        assert!(
            !response.starts_with("HTTP/1.1 100 Continue"),
            "response was {response:?}"
        );
        assert_eq!(body(&response), "Hello, World!\n");
    }

    #[test]
    fn absolute_form_target_is_routed_by_path() {
        let (response, info) = run(
            "GET http://localhost/ HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        let info = info.expect("served");
        assert_eq!(info.status, "200 OK");
        assert_eq!(info.path, "/");
        assert_eq!(body(&response), "Hello, World!\n");
    }

    #[test]
    fn absolute_form_without_path_serves_root() {
        let (_, info) = run(
            "GET http://localhost HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "200 OK");
    }

    #[test]
    fn origin_path_strips_scheme_and_authority() {
        assert_eq!(origin_path("http://example.com/a?b#c"), "/a?b#c");
        assert_eq!(origin_path("https://example.com"), "/");
        assert_eq!(origin_path("HTTP://example.com/x"), "/x");
        assert_eq!(origin_path("/plain?q=1"), "/plain?q=1");
        assert_eq!(origin_path("*"), "*");
    }

    #[test]
    fn is_token_matches_rfc_tokens() {
        assert!(is_token("Host"));
        assert!(is_token("x-!#$%&'*+-.^_`|~"));
        assert!(!is_token(""));
        assert!(!is_token("X A"));
        assert!(!is_token("X:"));
    }

    #[test]
    fn non_token_method_is_bad_request() {
        let (_, info) = run(
            "GET@ / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn control_character_in_target_is_bad_request() {
        let (_, info) = run(
            "GET /\u{1} HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn empty_path_targets_serve_root() {
        for request in [
            "GET http://localhost?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "GET ?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "GET http://localhost#f HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ] {
            let (response, info) = run(request, &default_config());
            let info = info.expect("served");
            assert_eq!(info.status, "200 OK", "was {request:?}");
            assert_eq!(info.path, "/", "was {request:?}");
            assert_eq!(body(&response), "Hello, World!\n");
        }
    }

    #[test]
    fn head_stalled_inside_the_request_line_has_no_body() {
        let request = "HEAD / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        // Four trickled bytes spell `HEAD`, so the method is known before the
        // deadline expires inside the request line.
        let mut reader = BufReader::new(Trickle::new(request.as_bytes().to_vec()));
        let (response, info) = run_deadline(
            &mut reader,
            &default_config(),
            &mut CountdownDeadline { arms: 4 },
        );
        let info = info.expect("served");
        assert_eq!(info.status, "408 Request Timeout");
        assert_eq!(header_value(&response, "Content-Length"), Some("16"));
        assert_eq!(body(&response), "");
    }

    #[test]
    fn head_timeout_has_no_body() {
        let request = "HEAD / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        // The request line is trickled and read in full, so `HEAD` is known,
        // before the deadline expires while the headers are read.
        let line_arms = request.find("\r\n").expect("line") + 2;
        let mut reader = BufReader::new(Trickle::new(request.as_bytes().to_vec()));
        let (response, info) = run_deadline(
            &mut reader,
            &default_config(),
            &mut CountdownDeadline { arms: line_arms },
        );
        let info = info.expect("served");
        assert_eq!(info.status, "408 Request Timeout");
        assert_eq!(body(&response), "");
        assert_eq!(header_value(&response, "Content-Length"), Some("16"));
    }

    #[test]
    fn head_uri_too_long_has_no_body() {
        let request = format!("HEAD /{}\r\n", "a".repeat(9000));
        let (response, info) = run(&request, &default_config());
        let info = info.expect("served");
        assert_eq!(info.status, "414 URI Too Long");
        assert_eq!(info.method, "HEAD");
        assert_eq!(body(&response), "");
    }

    #[test]
    fn conflicting_oversized_content_lengths_are_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nContent-Length: 99999999999999999999\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn truncated_body_is_bad_request() {
        let (_, info) = run(
            "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nab",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn header_line_over_limit_is_431() {
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nX: {}\r\n\r\n",
            "a".repeat(9000)
        );
        let (_, info) = run(&request, &default_config());
        assert_eq!(
            info.expect("served").status,
            "431 Request Header Fields Too Large"
        );
    }

    #[test]
    fn header_bytes_over_limit_is_431() {
        let value = "a".repeat(MAX_HEADER_LINE - "X: ".len());
        let mut request = String::from("GET / HTTP/1.1\r\nHost: localhost\r\n");
        for _ in 0..9 {
            request.push_str("X: ");
            request.push_str(&value);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        let (_, info) = run(&request, &default_config());
        assert_eq!(
            info.expect("served").status,
            "431 Request Header Fields Too Large"
        );
    }

    #[test]
    fn header_bytes_are_counted_on_the_wire() {
        // Three maximal header lines of raw `0xFF` bytes are under 64 KiB on the
        // wire, but would exceed it if the lossy UTF-8 expansion were counted.
        let pad = MAX_HEADER_LINE - "X-Pad: ".len();
        let mut request = b"GET / HTTP/1.1\r\nHost: localhost\r\n".to_vec();
        for _ in 0..3 {
            request.extend_from_slice(b"X-Pad: ");
            request.resize(request.len() + pad, 0xff);
            request.extend_from_slice(b"\r\n");
        }
        request.extend_from_slice(b"\r\n");
        assert!(request.len() < MAX_HEADER_BYTES);
        let mut reader = BufReader::new(Cursor::new(request));
        let mut writer = Vec::new();
        let info = handle(&mut reader, &mut writer, &default_config(), &mut ())
            .expect("handle must succeed")
            .expect("served");
        assert_eq!(info.status, "200 OK");
        assert_eq!(
            body(&String::from_utf8(writer).expect("utf-8")),
            "Hello, World!\n"
        );
    }

    #[test]
    fn http12_is_served_as_11() {
        let (response, info) = run(
            "GET / HTTP/1.2\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "200 OK");
        assert_eq!(body(&response), "Hello, World!\n");
    }

    #[test]
    fn http20_is_505() {
        let (_, info) = run(
            "GET / HTTP/2.0\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(
            info.expect("served").status,
            "505 HTTP Version Not Supported"
        );
    }

    #[test]
    fn asterisk_form_is_400() {
        let (_, info) = run(
            "GET * HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        assert_eq!(info.expect("served").status, "400 Bad Request");
    }

    #[test]
    fn options_asterisk_is_405() {
        let (response, info) = run(
            "OPTIONS * HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        let info = info.expect("served");
        assert_eq!(info.status, "405 Method Not Allowed");
        assert_eq!(header_value(&response, "Allow"), Some("GET, HEAD"));
    }

    #[test]
    fn query_and_fragment_are_stripped() {
        let (response, info) = run(
            "GET /?x=1#f HTTP/1.1\r\nHost: localhost\r\n\r\n",
            &default_config(),
        );
        let info = info.expect("served");
        assert_eq!(info.status, "200 OK");
        assert_eq!(info.path, "/");
        assert_eq!(body(&response), "Hello, World!\n");
    }
}
