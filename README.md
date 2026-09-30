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
  -p, --port <PORT>    TCP port to listen on (default: 21212)
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
thread, with at most 256 concurrent connections; connections beyond that limit receive
`503 Service Unavailable` with a `Retry-After: 1` header. Each connection has a 10s read timeout and a
10s write timeout. One line is logged to stdout per request (method, path, resolved language, status).

### Request limits

Requests that exceed the parser's limits are rejected with a short plain-text body and the status below:

| Condition                                                   | Status                              |
| ----------------------------------------------------------- | ----------------------------------- |
| Request line longer than 8 KiB                               | `414 URI Too Long`                  |
| Headers larger than 64 KiB in total, or more than 100 fields | `431 Request Header Fields Too Large` |
| Request body larger than 8 MiB                               | `413 Payload Too Large`             |
| Body sent with `Transfer-Encoding` (chunked)                  | `501 Not Implemented`               |
| HTTP version other than `1.0` or `1.1`                       | `505 HTTP Version Not Supported`     |
| Malformed request line                                      | `400 Bad Request`                   |

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
