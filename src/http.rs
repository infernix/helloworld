//! A tiny dependency-free HTTP server: one thread per connection.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use crate::i18n;

/// Largest request body we are willing to drain before rejecting the request.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Runtime configuration derived from the command line.
pub struct Config {
    /// Language forced by `--lang` (already validated/normalized), if any.
    pub lang: Option<&'static str>,
    /// Custom greeting from `--text`; replaces every greeting when present.
    pub text: Option<String>,
}

/// Accept connections forever, handling each one on its own thread.
pub fn serve(listener: TcpListener, config: Arc<Config>) {
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let config = Arc::clone(&config);
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, &config) {
                        eprintln!("helloworld: connection error: {err}");
                    }
                });
            }
            Err(err) => eprintln!("helloworld: accept failed: {err}"),
        }
    }
}

fn handle_connection(mut stream: TcpStream, config: &Config) -> std::io::Result<()> {
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);

    let mut line = String::new();

    // Skip any leading blank lines, then read the request line.
    let request_line = loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if !trimmed.is_empty() {
            break trimmed.to_string();
        }
    };

    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or("").to_string();
    let target = request_parts.next().unwrap_or("").to_string();
    let version = request_parts.next().unwrap_or("").to_string();

    if method.is_empty() || target.is_empty() || version.is_empty() {
        respond(&mut stream, "400 Bad Request", "bad request\n")?;
        let shown = if target.is_empty() {
            "-"
        } else {
            target.as_str()
        };
        log(&method, shown, "-", "400 Bad Request");
        return Ok(());
    }

    // Read headers until the blank line that ends the header block.
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    // Drain any request body so keep-alive connections stay sane.
    if let Some(value) = header(&headers, "content-length") {
        match value.parse::<usize>() {
            Ok(len) if len <= MAX_BODY => {
                let mut sink = vec![0_u8; len];
                reader.read_exact(&mut sink)?;
            }
            _ => {
                respond(&mut stream, "400 Bad Request", "bad request\n")?;
                log(&method, &target, "-", "400 Bad Request");
                return Ok(());
            }
        }
    }

    let path = target.split(['?', '#']).next().unwrap_or("").to_string();

    let (status, lang_label, body) = if method != "GET" {
        ("404 Not Found", "-".to_string(), "not found\n".to_string())
    } else {
        match path.as_str() {
            "/" => {
                let (lang_label, body) = greeting(config, &headers);
                ("200 OK", lang_label, body)
            }
            "/healthz" => ("200 OK", "-".to_string(), "ok\n".to_string()),
            _ => ("404 Not Found", "-".to_string(), "not found\n".to_string()),
        }
    };

    respond(&mut stream, status, &body)?;
    log(&method, &path, &lang_label, status);
    Ok(())
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

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

fn log(method: &str, path: &str, lang: &str, status: &str) {
    println!("{} {} lang={} status={}", method, path, lang, status);
    let _ = std::io::stdout().flush();
}
