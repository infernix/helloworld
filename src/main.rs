//! `helloworld` — a multilingual hello-world HTTP server with zero dependencies.

#![forbid(unsafe_code)]

mod http;
mod i18n;

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::process;
use std::sync::Arc;

/// Default TCP port when `--port` is not given.
const DEFAULT_PORT: u16 = 21212;

/// Default bind address when `--host` is not given.
const DEFAULT_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Version reported by `--version`, taken from the crate manifest.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Largest accepted `--text` value, in bytes.
const MAX_TEXT: usize = 8 * 1024;

/// Resolved options for a normal server run.
#[derive(Debug, PartialEq, Eq)]
struct Options {
    /// IP address to bind.
    host: IpAddr,
    /// TCP port to listen on; `0` asks the OS for any free port.
    port: u16,
    /// Language code for the greeting, if one was requested.
    lang: Option<&'static str>,
    /// Custom greeting text, if one was requested.
    text: Option<String>,
}

/// What the command line asked the program to do.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Start the server with these options.
    Run(Options),
    /// Print the usage text and exit.
    Help,
    /// Print the supported language table and exit.
    ListLangs,
    /// Print the version and exit.
    Version,
}

/// Build the `--help` text.
fn usage() -> String {
    format!(
        "\
Usage: helloworld [OPTIONS]

A tiny multilingual hello-world HTTP server.

Options:
  -l, --lang <CODE>    Language code for the greeting (default: en)
  -t, --text <STRING>  Custom greeting, replaces the greeting entirely (max {MAX_TEXT} bytes)
  -p, --port <PORT>    TCP port (default: {DEFAULT_PORT}, 0 = any free port)
      --host <IP>      IP address to bind (default: {DEFAULT_HOST})
      --list-langs     List supported language codes and exit
  -V, --version        Print version and exit
  -h, --help           Print this help and exit

Flags accept both `--flag value` and `--flag=value` forms.
Supported language codes: {codes}

Endpoints:
  GET /         the greeting, as text/plain; charset=utf-8
  GET /healthz  `ok`

Examples:
  helloworld --lang nl
  helloworld --text 'Bonjour le monde!' --port 21213
  helloworld --host 0.0.0.0 --port 8080
  curl -s -H 'Accept-Language: de-DE,de;q=0.9' http://127.0.0.1:{DEFAULT_PORT}/
",
        DEFAULT_PORT = DEFAULT_PORT,
        MAX_TEXT = MAX_TEXT,
        DEFAULT_HOST = DEFAULT_HOST,
        codes = i18n::supported_codes()
    )
}

/// Write `text` to stdout, ignoring I/O errors so a closed pipe never panics.
fn print_stdout(text: &str) {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(text.as_bytes());
    let _ = handle.flush();
}

/// Fetch the value of `name`, either inline (`--flag=value`) or from the next argument.
fn flag_value(
    argv: &[String],
    index: &mut usize,
    name: &str,
    inline: Option<&str>,
) -> Result<String, String> {
    if let Some(value) = inline {
        return Ok(value.to_string());
    }
    *index += 1;
    match argv.get(*index) {
        Some(value) => Ok(value.clone()),
        None => Err(format!("missing value for {name}")),
    }
}

/// Strip one pair of surrounding brackets, so the `[::1]` form the startup line
/// prints can be passed back to `--host`.
fn unbracket(value: &str) -> &str {
    value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(value)
}

/// Parse `argv` (program name already stripped) into a [`Command`].
fn parse_args(argv: &[String]) -> Result<Command, String> {
    let mut options = Options {
        host: DEFAULT_HOST,
        port: DEFAULT_PORT,
        lang: None,
        text: None,
    };
    let mut index = 0;

    while index < argv.len() {
        let arg = &argv[index];
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg.as_str(), None),
        };

        match name {
            "--" => {}
            "-h" | "--help" => return Ok(Command::Help),
            "--list-langs" => return Ok(Command::ListLangs),
            "-V" | "--version" => return Ok(Command::Version),
            "-l" | "--lang" => {
                let value = flag_value(argv, &mut index, name, inline)?;
                match i18n::lookup(&value) {
                    Some(lang) => options.lang = Some(lang.code),
                    None => {
                        return Err(format!(
                            "unsupported language code '{}'; supported codes: {}",
                            value.escape_debug(),
                            i18n::supported_codes()
                        ));
                    }
                }
            }
            "-t" | "--text" => {
                let value = flag_value(argv, &mut index, name, inline)?;
                if value.len() > MAX_TEXT {
                    return Err(format!("--text is longer than {MAX_TEXT} bytes"));
                }
                options.text = Some(value);
            }
            "-p" | "--port" => {
                let value = flag_value(argv, &mut index, name, inline)?;
                match value.parse::<u16>() {
                    Ok(port) => options.port = port,
                    Err(_) => return Err(format!("invalid port '{}'", value.escape_debug())),
                }
            }
            "--host" => {
                let value = flag_value(argv, &mut index, name, inline)?;
                match unbracket(&value).parse::<IpAddr>() {
                    Ok(host) => options.host = host,
                    Err(_) => {
                        return Err(format!(
                            "invalid host '{}' (expected an IP address)",
                            value.escape_debug()
                        ));
                    }
                }
            }
            other => return Err(format!("unknown argument '{}'", other.escape_debug())),
        }

        index += 1;
    }

    Ok(Command::Run(options))
}

/// Lossily convert process arguments to `String`s.
///
/// Command-line arguments are OS strings and need not be valid UTF-8; a
/// non-UTF-8 argument is replaced with `U+FFFD` rather than aborting the
/// process.
fn collect_args<I>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = OsString>,
{
    args.into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// Entry point: parse the command line, then run, print or fail.
fn main() {
    let argv = collect_args(env::args_os().skip(1));

    let command = match parse_args(&argv) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("helloworld: {message}");
            eprintln!("try 'helloworld --help' for usage");
            process::exit(2);
        }
    };

    let options = match command {
        Command::Help => {
            print_stdout(&usage());
            return;
        }
        Command::ListLangs => {
            print_stdout(&i18n::list_langs());
            return;
        }
        Command::Version => {
            print_stdout(&format!("helloworld {VERSION}\n"));
            return;
        }
        Command::Run(options) => options,
    };

    let addr = SocketAddr::new(options.host, options.port);
    let listener = match TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("helloworld: failed to bind {addr}: {err}");
            process::exit(1);
        }
    };

    let active = match &options.text {
        Some(_) => "custom (--text)".to_string(),
        None => options.lang.unwrap_or(i18n::DEFAULT_CODE).to_string(),
    };
    let bound = listener.local_addr().unwrap_or(addr);
    print_stdout(&format!(
        "helloworld listening on http://{bound} (language: {active})\n"
    ));

    let config = Arc::new(http::Config {
        lang: options.lang,
        text: options.text,
    });
    http::serve(listener, config);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an argv vector from string literals.
    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    /// Options with every field left at its default.
    fn defaults() -> Options {
        Options {
            host: DEFAULT_HOST,
            port: DEFAULT_PORT,
            lang: None,
            text: None,
        }
    }

    /// Run `parse_args` and unwrap the [`Command::Run`] options.
    fn run_options(args: &[&str]) -> Options {
        match parse_args(&argv(args)) {
            Ok(Command::Run(options)) => options,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn empty_argv_yields_defaults() {
        assert_eq!(parse_args(&argv(&[])), Ok(Command::Run(defaults())));
    }

    #[test]
    fn defaults_are_localhost_and_21212() {
        let options = run_options(&[]);
        assert_eq!(options.host, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(options.port, 21212);
        assert_eq!(options.lang, None);
        assert_eq!(options.text, None);
    }

    #[test]
    fn lang_flag_sets_language() {
        assert_eq!(run_options(&["--lang", "nl"]).lang, Some("nl"));
    }

    #[test]
    fn inline_short_lang_flag_is_case_insensitive() {
        assert_eq!(run_options(&["-l=NL"]).lang, Some("nl"));
    }

    #[test]
    fn last_lang_flag_wins() {
        assert_eq!(run_options(&["--lang", "nl", "-l", "de"]).lang, Some("de"));
    }

    #[test]
    fn unsupported_language_is_an_error() {
        let err = parse_args(&argv(&["--lang", "klingon"])).unwrap_err();
        assert!(
            err.starts_with("unsupported language code 'klingon'; supported codes: "),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn port_flag_sets_port() {
        assert_eq!(run_options(&["--port", "8080"]).port, 8080);
    }

    #[test]
    fn zero_port_selects_an_ephemeral_port() {
        assert_eq!(run_options(&["--port", "0"]).port, 0);
    }

    #[test]
    fn port_above_u16_range_is_an_error() {
        assert_eq!(
            parse_args(&argv(&["--port", "65536"])),
            Err("invalid port '65536'".to_string())
        );
    }

    #[test]
    fn collect_args_replaces_invalid_utf8_and_keeps_valid() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let args = vec![
                OsString::from("--lang"),
                OsString::from_vec(vec![0xff, 0xfe]),
            ];
            assert_eq!(
                collect_args(args),
                vec!["--lang".to_string(), "\u{fffd}\u{fffd}".to_string()]
            );
        }
        #[cfg(not(unix))]
        {
            let args = vec![OsString::from("--lang")];
            assert_eq!(collect_args(args), vec!["--lang".to_string()]);
        }
    }

    #[test]
    fn out_of_range_port_is_an_error() {
        assert_eq!(
            parse_args(&argv(&["--port", "70000"])),
            Err("invalid port '70000'".to_string())
        );
    }

    #[test]
    fn host_flag_sets_address() {
        assert_eq!(
            run_options(&["--host", "0.0.0.0"]).host,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn hostname_is_an_error() {
        assert_eq!(
            parse_args(&argv(&["--host", "localhost"])),
            Err("invalid host 'localhost' (expected an IP address)".to_string())
        );
    }

    #[test]
    fn action_flags_return_their_command() {
        assert_eq!(parse_args(&argv(&["--help"])), Ok(Command::Help));
        assert_eq!(parse_args(&argv(&["-h"])), Ok(Command::Help));
        assert_eq!(parse_args(&argv(&["--list-langs"])), Ok(Command::ListLangs));
        assert_eq!(parse_args(&argv(&["--version"])), Ok(Command::Version));
        assert_eq!(parse_args(&argv(&["-V"])), Ok(Command::Version));
    }

    #[test]
    fn first_action_flag_wins() {
        assert_eq!(
            parse_args(&argv(&["--version", "--help"])),
            Ok(Command::Version)
        );
    }

    #[test]
    fn trailing_text_flag_is_an_error() {
        assert_eq!(
            parse_args(&argv(&["--text"])),
            Err("missing value for --text".to_string())
        );
    }

    #[test]
    fn empty_text_is_an_empty_greeting() {
        assert_eq!(run_options(&["--text", ""]).text, Some(String::new()));
        assert_eq!(run_options(&["--text="]).text, Some(String::new()));
    }

    #[test]
    fn empty_language_is_an_error() {
        let err = parse_args(&argv(&["--lang", ""])).unwrap_err();
        assert!(
            err.starts_with("unsupported language code ''"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn empty_port_is_an_error() {
        assert_eq!(
            parse_args(&argv(&["--port="])),
            Err("invalid port ''".to_string())
        );
    }

    #[test]
    fn bracketed_ipv6_host_is_accepted() {
        assert_eq!(
            run_options(&["--host", "[::1]"]).host,
            "::1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn end_of_options_marker_is_ignored() {
        assert_eq!(run_options(&["--", "--lang", "nl"]).lang, Some("nl"));
    }

    #[test]
    fn text_flag_sets_custom_greeting() {
        assert_eq!(run_options(&["--text", "Hi"]).text, Some("Hi".to_string()));
    }

    #[test]
    fn text_of_exactly_max_length_is_accepted() {
        let value = "a".repeat(MAX_TEXT);
        assert_eq!(run_options(&["--text", value.as_str()]).text, Some(value));
    }

    #[test]
    fn text_longer_than_max_length_is_an_error() {
        let value = "a".repeat(MAX_TEXT + 1);
        assert_eq!(
            parse_args(&argv(&["--text", value.as_str()])),
            Err(format!("--text is longer than {MAX_TEXT} bytes"))
        );
    }

    #[test]
    fn unknown_argument_is_an_error() {
        assert_eq!(
            parse_args(&argv(&["--nope"])),
            Err("unknown argument '--nope'".to_string())
        );
    }
}
