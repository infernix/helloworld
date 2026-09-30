//! `helloworld` — a multilingual hello-world HTTP server with zero dependencies.

mod http;
mod i18n;

use std::env;
use std::io::Write;
use std::net::TcpListener;
use std::process;
use std::sync::Arc;

/// Default TCP port when `--port` is not given.
const DEFAULT_PORT: u16 = 21212;

fn usage() -> String {
    format!(
        "\
Usage: helloworld [OPTIONS]

A tiny multilingual hello-world HTTP server.

Options:
  -l, --lang <CODE>    Language code for the greeting (default: en)
  -t, --text <STRING>  Custom greeting, replaces the greeting entirely
  -p, --port <PORT>    TCP port to listen on (default: {DEFAULT_PORT})
      --list-langs     List supported language codes and exit
  -h, --help          Print this help and exit

Flags accept both `--flag value` and `--flag=value` forms.
Supported language codes: {codes}

Endpoints:
  GET /         the greeting, as text/plain; charset=utf-8
  GET /healthz  `ok`

Examples:
  helloworld --lang nl
  helloworld --text 'Bonjour le monde!' --port 21213
  curl -s -H 'Accept-Language: de-DE,de;q=0.9' http://127.0.0.1:{DEFAULT_PORT}/
",
        DEFAULT_PORT = DEFAULT_PORT,
        codes = i18n::supported_codes()
    )
}

/// Parsed command line.
struct Options {
    lang: Option<&'static str>,
    text: Option<String>,
    port: u16,
}

fn fail(message: &str) -> ! {
    eprintln!("helloworld: {message}");
    eprintln!("try 'helloworld --help' for usage");
    process::exit(2);
}

/// Fetch the value of `name`, either inline (`--flag=value`) or from the next arg.
fn flag_value(argv: &[String], index: &mut usize, name: &str, inline: Option<String>) -> String {
    if let Some(value) = inline {
        if value.is_empty() {
            fail(&format!("missing value for {name}"));
        }
        return value;
    }
    *index += 1;
    match argv.get(*index) {
        Some(value) => value.clone(),
        None => fail(&format!("missing value for {name}")),
    }
}

fn parse_args(argv: &[String]) -> Options {
    let mut options = Options {
        lang: None,
        text: None,
        port: DEFAULT_PORT,
    };
    let mut index = 0;

    while index < argv.len() {
        let arg = argv[index].clone();
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };

        match name.as_str() {
            "-h" | "--help" => {
                print!("{}", usage());
                let _ = std::io::stdout().flush();
                process::exit(0);
            }
            "--list-langs" => {
                print!("{}", i18n::list_langs());
                let _ = std::io::stdout().flush();
                process::exit(0);
            }
            "-l" | "--lang" => {
                let value = flag_value(argv, &mut index, &name, inline);
                match i18n::lookup(&value) {
                    Some(lang) => options.lang = Some(lang.code),
                    None => {
                        eprintln!("helloworld: unsupported language code '{value}'");
                        eprintln!("supported codes: {}", i18n::supported_codes());
                        process::exit(2);
                    }
                }
            }
            "-t" | "--text" => {
                options.text = Some(flag_value(argv, &mut index, &name, inline));
            }
            "-p" | "--port" => {
                let value = flag_value(argv, &mut index, &name, inline);
                match value.parse::<u16>() {
                    Ok(port) if port != 0 => options.port = port,
                    _ => fail(&format!("invalid port '{value}'")),
                }
            }
            other => fail(&format!("unknown argument '{other}'")),
        }

        index += 1;
    }

    options
}

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();
    let options = parse_args(&argv);

    let listener = match TcpListener::bind(("127.0.0.1", options.port)) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!(
                "helloworld: failed to bind 127.0.0.1:{}: {err}",
                options.port
            );
            process::exit(1);
        }
    };

    let addr = match listener.local_addr() {
        Ok(addr) => addr.to_string(),
        Err(_) => format!("127.0.0.1:{}", options.port),
    };

    let active = match &options.text {
        Some(_) => "custom (--text)".to_string(),
        None => options.lang.unwrap_or(i18n::DEFAULT_CODE).to_string(),
    };

    println!("helloworld listening on http://{addr} (language: {active})");
    let _ = std::io::stdout().flush();

    let config = Arc::new(http::Config {
        lang: options.lang,
        text: options.text,
    });
    http::serve(listener, config);
}
