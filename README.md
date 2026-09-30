# helloworld

A tiny multilingual "hello world" HTTP server written in Rust, using **only the standard library**
(no external crates, no network access needed to build).

It serves a greeting in any of 22 languages, negotiated from the command line or from the request's
`Accept-Language` header.

## Build

```sh
cargo build --release
```

The binary lands at `./target/release/helloworld`.

The minimum supported Rust version is 1.74 (declared as `rust-version` in `Cargo.toml`); CI builds the
crate with that toolchain and fails if the workflow's toolchain and the manifest's `rust-version` disagree,
so the MSRV claim is verified on every run.

## Usage

```
Usage: helloworld [OPTIONS]

A tiny multilingual hello-world HTTP server.

Options:
  -l, --lang <CODE>    Language code for the greeting (default: en)
  -t, --text <STRING>  Custom greeting, replaces the greeting entirely (max 8192 bytes)
  -p, --port <PORT>    TCP port (default: 21212, 0 = any free port)
      --host <IP>      IP address to bind (default: 127.0.0.1)
      --list-langs     List supported language codes and exit
  -V, --version        Print version and exit
  -h, --help           Print this help and exit

Flags accept both `--flag value` and `--flag=value` forms.
Supported language codes: ar, cs, da, de, el, en, es, fi, fr, he, it, ja, ko, nb, nl, pl, pt, ru, sv, tr, uk, zh

Endpoints:
  GET /         the greeting, as text/plain; charset=utf-8
  GET /healthz  `ok`

Examples:
  helloworld --lang nl
  helloworld --text 'Bonjour le monde!' --port 21213
  helloworld --host 0.0.0.0 --port 8080
  curl -s -H 'Accept-Language: de-DE,de;q=0.9' http://127.0.0.1:21212/
```

Flags accept both `--flag value` and `--flag=value` forms; a bare `--` is accepted and ignored. Unknown
flags and flags whose value is missing exit non-zero with a message on stderr. An explicitly empty value is
accepted: `--text ''` serves an empty greeting, while `--lang ''`, `--port ''` and `--host ''` fail their own
validation with a message that names the real cause. An unsupported language code also exits non-zero, prints the
error on stderr and lists the supported codes. A `--text` value longer than 8 KiB is rejected as a usage
error: the program exits with status 2 and prints the error on stderr. `--host` accepts an IP address, with an
optional pair of `[` `]` around an IPv6 address.

The server binds `<host>:<port>` (default `127.0.0.1:21212`) and handles each connection on its own
thread, with at most 256 connections served at once. Connections beyond that limit are answered with
`503 Service Unavailable`, a `Retry-After: 1` header and no body (so the rejection is also correct for
a `HEAD` request); up to 64 rejections run concurrently on their
own threads, so a slow client cannot stall the accept loop, and any further connections are closed
immediately. Each rejection is also recorded as a `status=503` access-log line, and rejections are budgeted
separately from served connections, so a burst of rejections cannot consume the capacity meant for served
requests. Each connection uses a single file descriptor, so the 256-connection cap fits inside a
default 1024-descriptor limit. Each connection has a 10s read timeout and a 10s write timeout, and the
10s request deadline is enforced on every socket read, so a client that trickles the request head or body
one byte at a time is still answered with `408` within 10 s. After the response is written
the write side is half-closed and the read side drained for at most 500 ms in total before the socket is
closed. On startup one line is printed with the address actually bound:
`helloworld listening on http://127.0.0.1:<port> (language: <en|custom (--text)|<code>>)`.

Passing `--port 0` asks the OS for any free port; the startup line then prints the real port that was
assigned, which is useful for tests and for scripts that must not collide with an existing service.

### Request limits

Requests that exceed the parser's limits are rejected with a short plain-text body and the status below:

| Condition                                                   | Status                              |
| ----------------------------------------------------------- | ----------------------------------- |
| Request line longer than 8 KiB                               | `414 URI Too Long`                  |
| Single header line longer than 8 KiB, headers larger than 64 KiB in total, or more than 100 fields | `431 Request Header Fields Too Large` |
| Request body larger than 8 MiB, or a `Content-Length` above 8 MiB (checked before the body is read) | `413 Payload Too Large` |
| Any request carrying a `Transfer-Encoding` header               | `501 Not Implemented`               |
| Version token that is not `HTTP/...`                          | `400 Bad Request`                   |
| `HTTP` major version other than 1, or an `HTTP/1.x` minor that is empty or not numeric | `505 HTTP Version Not Supported` |
| Malformed request line, method or request target              | `400 Bad Request`                   |
| Asterisk-form target (`*`) on a method other than `OPTIONS`    | `400 Bad Request`                   |
| HTTP/1.1 request without exactly one valid `Host` header      | `400 Bad Request`                   |
| `Host` field value that is not a valid `uri-host [":" port ]` (RFC 9110 7.2) | `400 Bad Request`                   |
| `Content-Length` that is not a plain decimal number, or two conflicting values (even when one of them overflows) | `400 Bad Request`                   |
| Malformed header line: no colon, whitespace before the colon, a non-token field name, an obsolete folded continuation line, or a control character in the value | `400 Bad Request` |
| `Accept-Encoding` that refuses the `identity` coding (RFC 9110 12.5.3) | `406 Not Acceptable`                |
| `Expect` header with a value other than `100-continue`        | `417 Expectation Failed`            |
| Request head and body not complete within 10 s                  | `408 Request Timeout`               |

`Expect: 100-continue` is answered with `100 Continue` only for HTTP/1.1 requests; in an HTTP/1.0
request the expectation is ignored (RFC 7231) and the body is read as usual. Absolute-form request
targets such as `GET http://example.com/ HTTP/1.1` are accepted (RFC 7230); routing and logging use
the path.

A higher `HTTP/1.x` minor version such as `HTTP/1.2` is processed as HTTP/1.1 (RFC 7230 2.6).

The parser is lenient in a few places: a bare LF is accepted as a line terminator, EOF inside the header
block ends the head (the request is served as if the blank line had arrived), and a duplicate `Host` is rejected
for HTTP/1.0 as well as HTTP/1.1. Multiple `Accept-Language` field lines are combined in order, as if they had
been sent as one comma-separated list (RFC 9110 5.3). Each `q` weight must match the RFC 9110 `qvalue` grammar
`qvalue = ( "0" [ "." 0*3DIGIT ] ) / ( "1" [ "." 0*3("0") ] )`, so `0`, `0.5`, `0.125`, `1` and `1.000` are
accepted while `.5`, `00.5`, `+0.5`, `1e-1`, `0.0001`, `0.1234` and `1.5` are not; a malformed weight drops that
entry, and so does a tag that is not a `language-range` (RFC 4647 2.1). A `Host` field value must be a
`uri-host [":" port ]` (RFC 9110 7.2), so `user@host` (userinfo), `[::1` (an unterminated IP-literal) and `%zz`
(a bad percent-escape) are rejected with `400 Bad Request`.

### Access log

For each served request one line is queued for stdout, in this exact format:

```
{ISO8601-UTC} {peer|"-"} {method} {path} lang={lang} status={code} dur={ms:.1}ms
```

The fields are, in order: the ISO-8601 UTC timestamp, the peer address (or `-` when unavailable), the
request method, the request path, the resolved language, the response status code, and the time spent
serving the request in milliseconds with one decimal.

The timestamp is the moment the line is written, and the method, path and language fields are sanitized before
they are logged: control characters become `?`, and a field longer than 120 characters is truncated with `...`. A
rejected connection is logged as a `status=503` line with `-` as its method, path and language.

For example:

```
2026-09-30T11:15:00Z 127.0.0.1:54321 GET / lang=en status=200 dur=0.4ms
```

The startup line and the access log are written to stdout; usage errors, bind, accept and spawn
failures, and connection errors that are not client disconnects are written to stderr. A client that
disconnects mid-request is normal and is not logged as an error.

Access log lines pass through a bounded in-process queue that a single writer thread drains, so logging
never blocks the request path. When the log consumer stalls (for example a full pipe that nobody reads),
the queue fills and further lines are dropped rather than blocking a connection; the request is still
served and its response is unaffected.

## Endpoints

| Request                        | Response                                                                |
| ------------------------------ | ----------------------------------------------------------------------- |
| `GET /`, `HEAD /`              | `200 OK`, the greeting + newline, or `406 Not Acceptable`                 |
| `GET /healthz`, `HEAD /healthz` | `200 OK`, body `ok` and a newline                                        |
| any other path                 | `404 Not Found`, short plain-text body                                    |
| any other method               | `405 Method Not Allowed`, `Allow: GET, HEAD`, short plain-text body         |

`GET /` is answered with `406 Not Acceptable` when the request's `Accept-Language` header leaves no
catalog language acceptable (see [Greeting resolution precedence](#greeting-resolution-precedence)).

`HEAD` returns exactly the same status and headers as the corresponding `GET`, with no body. That holds for
error and timeout responses too: a `400`, `408`, `414`, `431`, `505` or `503` answered to a `HEAD` request
carries no body, and its `Content-Length` still matches the body the corresponding `GET` would have carried.

## Response headers

Every final response carries `Date` (IMF-fixdate, GMT), `Server: helloworld/<version>`,
`Content-Type: text/plain; charset=utf-8`, `Content-Length`, `Connection: close` and `Vary: Accept-Encoding`. The
`100 Continue` preliminary response is the exception: it carries only its status line.

Responses to `/` may carry one additional header:

- `Content-Language: <code>` — sent when the greeting came from the catalog, i.e. whenever `--text` was
  not given.

The `/` catalog responses — the greeting and the language `406` — extend `Vary` to
`Accept-Language, Accept-Encoding`, because the body depends on the request's `Accept-Language` header even when
it is absent. `--lang` and `--text` responses vary on `Accept-Encoding` only.

`/healthz` and every other response carry no other extra headers; the `405` carries `Allow: GET, HEAD` and the
`503` rejection carries `Retry-After: 1` and `Vary: Accept-Encoding`. Every response is written as `HTTP/1.1`, even
to an HTTP/1.0 request.

## Greeting resolution precedence

For every request to `/`, the greeting is resolved in this order:

1. `--text <STRING>` — if given, it replaces the greeting for **every** request, regardless of language.
2. `--lang <CODE>` — if given, that language is used for every request, ignoring request headers.
3. The request's `Accept-Language` header — the highest-quality catalog language wins, where an
   exact range beats the `*` wildcard and a language marked `q=0` is never served. A header that leaves
   no catalog language acceptable is answered `406 Not Acceptable`; an absent, empty or malformed header
   keeps the `en` default.
4. `en` — the default.

Independently of the language, a request that refuses the `identity` content coding — `Accept-Encoding: identity;q=0`,
or `*;q=0` with no `identity` entry — is answered `406 Not Acceptable` on every route, since the response carries
no content coding (RFC 9110 12.5.3).

A plain `curl http://127.0.0.1:21212/` with no language arguments or headers therefore still returns
`200 OK` with the English greeting.

Language codes are matched case-insensitively on the primary subtag, so `nl-NL` resolves to `nl`, `pt-BR` to
`pt`, `zh-CN` to `zh`, and `en-US` to `en`.

## Supported languages

| Code | Language       | Greeting            |
| ---- | -------------- | ------------------- |
| en   | English        | Hello, World!       |
| nl   | Nederlands     | Hallo, wereld!      |
| de   | Deutsch        | Hallo, Welt!        |
| fr   | Français       | Bonjour le monde !  |
| es   | Español        | ¡Hola, Mundo!       |
| it   | Italiano       | Ciao, mondo!        |
| pt   | Português      | Olá, mundo!         |
| pl   | Polski         | Witaj, świecie!     |
| sv   | Svenska        | Hej, världen!       |
| da   | Dansk          | Hej, verden!        |
| fi   | Suomi          | Hei, maailma!       |
| nb   | Norsk bokmål   | Hei, verden!        |
| ru   | Русский        | Привет, мир!        |
| uk   | Українська     | Привіт, світе!      |
| ja   | 日本語         | こんにちは世界！    |
| zh   | 中文           | 你好，世界！        |
| ko   | 한국어         | 안녕하세요, 세계!   |
| ar   | العربية        | مرحبا بالعالم!      |
| he   | עברית          | שלום, עולם!         |
| tr   | Türkçe         | Merhaba, Dünya!     |
| el   | Ελληνικά       | Γεια σου, κόσμε!    |
| cs   | Čeština        | Ahoj, světe!        |

`helloworld --list-langs` prints the same table as `code<TAB>native name` lines.

## Examples

```sh
# Help and the language table
helloworld --help
helloworld --list-langs

# Serve Dutch on the default port 21212
helloworld --lang nl
curl -s http://127.0.0.1:21212/
# -> Hallo, wereld!

# Listen on every interface
helloworld --host 0.0.0.0 --port 21212
curl -s http://127.0.0.1:21212/
# -> Hello, World!

# German is picked up from the request header
curl -s -H 'Accept-Language: de-DE,de;q=0.9' http://127.0.0.1:21212/
# -> Hallo, Welt!

# Health check
curl -s -i http://127.0.0.1:21212/healthz
# -> HTTP/1.1 200 OK ... ok followed by a newline

# Headers only, no body
curl -s -I http://127.0.0.1:21212/
# -> HTTP/1.1 200 OK ... Content-Length: 14

# Unknown path
curl -s -i http://127.0.0.1:21212/nope
# -> HTTP/1.1 404 Not Found ... not found

# A custom greeting wins over both --lang and any Accept-Language header
helloworld --text 'Bonjour le monde!' --port 21213
curl -s -H 'Accept-Language: ru' http://127.0.0.1:21213/
# -> Bonjour le monde!
```

## Testing

```sh
cargo test
```

This runs the unit tests and the `tests/cli.rs` integration suite, which starts the real binary on an
ephemeral port (`--port 0`) and speaks HTTP to it.

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the test suite in both the
debug and release profiles, and a rustdoc build with warnings denied on the stable toolchain, plus the test
suite and release build on the MSRV toolchain.

## License

MIT — see [LICENSE](LICENSE).
