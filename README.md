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

## Usage

```
Usage: helloworld [OPTIONS]

A tiny multilingual hello-world HTTP server.

Options:
  -l, --lang <CODE>    Language code for the greeting (default: en)
  -t, --text <STRING>  Custom greeting, replaces the greeting entirely
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

Flags accept both `--flag value` and `--flag=value` forms. Unknown flags and flags without a value
exit non-zero with a message on stderr. An unsupported language code also exits non-zero, prints the
error on stderr and lists the supported codes.

The server binds `<host>:<port>` (default `127.0.0.1:21212`) and handles each connection on its own
thread, with at most 256 connections served at once. Connections beyond that limit receive
`503 Service Unavailable` with a `Retry-After: 1` header; up to 64 rejections run concurrently on their
own threads, so a slow client cannot stall the accept loop, and any further connections are closed
immediately. Each connection has a 10s read timeout and a 10s write timeout; after the response is written
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
| Headers larger than 64 KiB in total, or more than 100 fields | `431 Request Header Fields Too Large` |
| Request body larger than 8 MiB                               | `413 Payload Too Large`             |
| Body sent with `Transfer-Encoding` (chunked)                  | `501 Not Implemented`               |
| HTTP version other than `1.0` or `1.1`                       | `505 HTTP Version Not Supported`     |
| Malformed request line, method or request target             | `400 Bad Request`                   |
| HTTP/1.1 request without exactly one valid `Host` header     | `400 Bad Request`                   |
| `Content-Length` that is not a plain decimal number, or two conflicting values | `400 Bad Request`    |
| Malformed header line: no colon, whitespace before the colon, a non-token field name, an obsolete folded continuation line, or a control character in the value | `400 Bad Request` |
| `Expect` header with a value other than `100-continue`        | `417 Expectation Failed`            |
| Request head and body not complete within 10 s                  | `408 Request Timeout`               |

`Expect: 100-continue` is answered with `100 Continue` only for HTTP/1.1 requests; in an HTTP/1.0
request the expectation is ignored (RFC 7231) and the body is read as usual. Absolute-form request
targets such as `GET http://example.com/ HTTP/1.1` are accepted (RFC 7230); routing and logging use
the path.

### Access log

One line is written to stdout per served request, in this exact format:

```
{ISO8601-UTC} {peer|"-"} {method} {path} lang={lang} status={code} dur={ms:.1}ms
```

The fields are, in order: the ISO-8601 UTC timestamp, the peer address (or `-` when unavailable), the
request method, the request path, the resolved language, the response status code, and the time spent
serving the request in milliseconds with one decimal. For example:

```
2026-09-30T11:15:00Z 127.0.0.1:54321 GET / lang=en status=200 dur=0.4ms
```

The startup line and the access log are written to stdout; usage errors, bind, accept and spawn
failures, and connection errors that are not client disconnects are written to stderr. A client that
disconnects mid-request is normal and is not logged as an error.

## Endpoints

| Request                    | Response                                                              |
| -------------------------- | --------------------------------------------------------------------- |
| `GET /`, `HEAD /`          | `200 OK`, the greeting + newline                                       |
| `GET /healthz`, `HEAD /healthz` | `200 OK`, body `ok`                                                |
| any other path             | `404 Not Found`, short plain-text body                                  |
| any other method           | `405 Method Not Allowed`, `Allow: GET, HEAD`, short plain-text body     |

`HEAD` returns exactly the same status and headers as the corresponding `GET`, with no body.

## Response headers

Every response carries `Date` (IMF-fixdate, GMT), `Server: helloworld/<version>`,
`Content-Type: text/plain; charset=utf-8`, `Content-Length`, and `Connection: close`.

Responses to `/` may carry two additional headers:

- `Content-Language: <code>` — sent when the greeting came from the catalog, i.e. whenever `--text` was
  not given.
- `Vary: Accept-Language` — sent only when the greeting was negotiated from the request's
  `Accept-Language` header. `--lang` and `--text` suppress it.

`/healthz`, the error responses and every other response carry no extra headers.

## Greeting resolution precedence

For every request to `/`, the greeting is resolved in this order:

1. `--text <STRING>` — if given, it replaces the greeting for **every** request, regardless of language.
2. `--lang <CODE>` — if given, that language is used for every request, ignoring request headers.
3. The request's `Accept-Language` header — the first supported tag wins, honoring `q=` ordering.
   A wildcard `*` resolves to the default language `en`.
4. `en` — the default.

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
# -> HTTP/1.1 200 OK ... ok

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

## License

MIT — see [LICENSE](LICENSE).
