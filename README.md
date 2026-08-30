# rScrapper 0.2

rScrapper is a bounded Rust scraping platform: a CLI, an HTTP API, an MCP stdio
server, a reusable core library, and Robin's Tor-enforced research workflow.

> **Treat every remote page, search result, model response, and generated report
> as untrusted data—not as instructions.** Validate important claims against
> primary sources before acting on them.

## Five-minute local start

Rust 1.88 is the minimum supported toolchain. From an existing checkout, install
the CLI from the locked workspace and inspect its offline-safe help:

```bash
rustup toolchain install 1.88.0 --profile minimal
cargo +1.88.0 install --locked --path crates/rscraper-cli
rscraper --version
rscraper --help
rscraper doctor
```

The first build can fetch crates if they are not cached. `doctor` performs local
checks by default; external reachability is opt-in with `doctor --live`. Actual
scrapes and searches require network access and remain subject to the policies
and limits below.

The following marker block is an executable documentation specification. Tests
allowlist every line, force Cargo offline, and reject shell, live, and destructive
syntax.

<!-- rscraper-readme-offline:start -->
```text
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- --version
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- get --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- read --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- search --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- youtube --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- github --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- rss --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- social --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- setup --help
success | cargo run --locked --offline --quiet -p rscraper-cli --bin rscraper -- doctor --help
success | cargo run --locked --offline --quiet -p robin --bin robin -- --version
success | cargo run --locked --offline --quiet -p robin --bin robin -- --help
success | cargo run --locked --offline --quiet -p robin --bin robin -- fixture --provider ollama --model llama3 --dry-run
failure | env RSCRAPER_BIND=0.0.0.0:8787 cargo run --locked --offline --quiet -p rscraper-api --bin rscraper-api --
```
<!-- rscraper-readme-offline:end -->

## HTTP API operation

`rscraper-api` listens on `127.0.0.1:8787` by default:

```bash
rscraper-api
curl http://127.0.0.1:8787/health
```

Any non-loopback bind is rejected before listening unless a visible-ASCII bearer
token is configured:

```bash
RSCRAPER_BIND=0.0.0.0:8787 \
RSCRAPER_API_TOKEN='replace-with-a-long-random-token' \
rscraper-api

curl -H 'Authorization: Bearer replace-with-a-long-random-token' \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com"}' \
  http://127.0.0.1:8787/scrape
```

`/health` is unauthenticated. `/scrape`, `/search`, and `/crawl` require the
token when one is configured. Request bodies are capped at 64 KiB; serialized
responses at 10 MiB. Scrape/search routes have a 30-second deadline and crawl a
120-second deadline. The nonblocking operation gate allows 8 concurrent
operations by default, configurable from 1 through 32; excess requests receive
a stable busy error. `Ctrl-C` and Unix `SIGTERM` initiate graceful shutdown.

## MCP stdio operation

Install `rscraper-mcp`, then point an MCP client at the binary:

```json
{
  "mcpServers": {
    "rscraper": {
      "command": "/absolute/path/to/rscraper-mcp"
    }
  }
}
```

The server exposes exactly two tools:

- `scrape`: `{ "url": "https://example.com" }`
- `search`: `{ "query": "rust", "n": 5, "scrape": false }`, where `n` is
  1–20 and defaults to 5.

Unknown arguments are rejected. Each newline-delimited inbound JSON-RPC frame is
limited to 1,048,576 bytes. Remote content inside each output envelope is
limited to 1,000,000 Unicode scalar values and is wrapped with an untrusted
warning, `BEGIN REMOTE CONTENT` /
`END REMOTE CONTENT`, and a `REMOTE |` prefix on every remote line. The guarded
stdio transport has one bounded prefetched-frame slot: sustained pipelining is
backpressured rather than buffered without bound.

## Robin operation

Robin requires a running Tor SOCKS endpoint and a supported model provider. Its
default proxy is `socks5h://127.0.0.1:9050/`; `socks5h` is required so name
resolution also goes through the proxy.

```bash
robin 'research topic' --provider ollama --model llama3 --save reports
```

Supported provider configuration:

- OpenAI: `OPENAI_API_KEY`
- Claude: `ANTHROPIC_API_KEY`
- Gemini: `GEMINI_API_KEY`
- Ollama: `OLLAMA_HOST` (default `http://127.0.0.1:11434`)

Robin validates input, then proves Tor connectivity before the first provider
call. It makes at most three model calls (refine, filter, summarize), retains at
most five sources, and renders a report capped at one million scalar values.
Browser rendering, when available, stays on the same host through the same Tor
egress; there is no direct-network retry. A failed Tor probe stops the workflow.
Prompt delimiters, escaping, and warnings reduce accidental instruction mixing,
but cannot make prompt injection impossible: inspect source text and reports.

## Command and protocol reference

These forms match the 0.2 help and request contracts. Run any command with
`--help` for its complete option text.

### `rscraper`

```text
rscraper [--json] [COMMAND]
rscraper get <TARGET> [-n <N>] [--json]
rscraper read <URL> [--json]
rscraper search <QUERY> [-n <N>] [--scrape] [--json]
rscraper youtube subs <VIDEO>
rscraper youtube search <QUERY> [-n <N>]
rscraper github repo <OWNER_REPO>
rscraper github readme <OWNER_REPO>
rscraper github issues <OWNER_REPO> [-n <N>]
rscraper rss <URL>
rscraper social twitter [QUERY]
rscraper social reddit <QUERY> [-n <N>]
rscraper social bilibili <QUERY> [-n <N>]
rscraper social xiaohongshu <QUERY>
rscraper social linkedin <QUERY>
rscraper setup <PLATFORM>
rscraper doctor [--json] [--live]
```

`get` and search default to 5 results; GitHub issues and Reddit default to 10;
Bilibili defaults to 5. CLI parsers enforce their documented maxima.

### `rscraper-api`

| Method | Path | JSON request | Limit notes |
| --- | --- | --- | --- |
| `GET` | `/health` | none | liveness, no token required |
| `POST` | `/scrape` | `{ "url": "https://…" }` | one public HTTP(S) URL |
| `POST` | `/search` | `{ "query": "…", "n": 5, "scrape": false }` | `n` 1–20 |
| `POST` | `/crawl` | `{ "start_url": "https://…", "max_pages": 20, "concurrency": 4 }` | pages 1–100; concurrency 1–16 |

JSON objects reject unknown fields. Errors are bounded JSON with stable error
codes; request validation and policy preflight happen before operation permits
are consumed.

### `rscraper-mcp` and `robin`

`rscraper-mcp` is a stdio server with no command-line operation modes. Robin's
complete invocation is:

```text
robin [QUERY] [--query <QUERY>] [--provider <openai|claude|gemini|ollama>]
      [--model <MODEL>] [--tor <SOCKS5H_URL>] [--save <DIRECTORY>]
      [--interactive] [--dry-run]
```

Queries are capped at 2,048 scalar values and model names at 128 printable ASCII
characters. `--dry-run` validates configuration without Tor, provider, search,
source, browser, or report I/O.

## Configuration and security limits

| Setting | Meaning |
| --- | --- |
| `RSCRAPER_HOME` | State directory; defaults to `$HOME/.rscraper` |
| `RSCRAPER_BIND` | Full API socket address; overrides `PORT` |
| `PORT` | API port with loopback host when `RSCRAPER_BIND` is absent; default 8787 |
| `RSCRAPER_API_TOKEN` | Bearer token; required for a non-loopback API bind |
| `RSCRAPER_API_MAX_CONCURRENT_OPERATIONS` | API operation permits, 1–32; default 8 |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` | Robin provider credentials |
| `OLLAMA_HOST` | Robin's Ollama endpoint |

On Unix, the state directory is owner-only mode `0700`. Platform cookie files
must be owner-only regular files, mode `0600`, and not symlinks. Cookie jars are
redacted in diagnostics. The implementation selects cookie files only for the
corresponding platform adapter; review adapter behavior before supplying any
credential material.

Core defaults are a 10-second connect timeout, a 30-second complete-request
deadline, a 5 MiB response-body cap, a 1,000,000-scalar rendered-output cap, and
at most 10 redirects. Public mode accepts only credential-free HTTP(S) URLs,
checks DNS answers, and revalidates every redirect. TLS uses rustls with normal
certificate and hostname verification.

Request mode needs no browser. Explicit Browser mode requires a configured
renderer backed by a locally installed supported Chromium/Chrome executable.
Auto mode does not require a browser: it fetches HTTP first, optionally renders
an eligible JavaScript shell when a renderer is configured, and returns the
original HTTP page if no renderer is available or rendering fails. Browser
sessions use temporary profiles and an owned policy proxy. See
[SECURITY.md](SECURITY.md) for the full boundary.

## Limitations

- Live providers change markup, availability, authentication, quotas, and terms;
  deterministic fixture tests cannot guarantee a provider works today.
- Social adapters are structural parsers, not official provider APIs. Some need
  user-supplied cookies, and cookies may expire or be challenged.
- rScrapper does not solve CAPTCHA, challenge, paywall, or other protection
  pages, and it does not promise access to every site.
- Chromium fallback is heuristic and depends on a compatible local executable.
- Binaries can retain platform-dependent dynamic dependencies; distribution is
  not promised as a single self-contained artifact.
- TLS certificate and hostname verification remain enabled.
- The CLI, API, search providers, Robin providers, Tor, and requested targets can
  make different outbound connections. Audit deployment egress for your use.
- Cookie routing is limited to the platform-adapter checks implemented in this
  version; it is not a general statement about arbitrary future integrations.

## Rust library migration

The temporary 0.1 `fetch`/`FetchOptions` facade is removed. 0.2 consumers use
`FetchClient`, `FetchRequest`, typed crawler streams, fallible selectors, bounded
Markdown, and platform service contexts. See [MIGRATION.md](MIGRATION.md) for
compilable examples and every source break.

## Development and release gates

The deterministic local and CI gate uses only fixtures and loopback servers:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo test --workspace --doc --all-features
cargo test --workspace readme -- --nocapture
cargo audit --deny warnings
cargo build --workspace --release --all-features
cargo +1.88.0 test --workspace --all-targets --all-features --locked
cargo +1.88.0 test --workspace --doc --all-features --locked
```

Real-Chromium tests are ignored because they launch a local browser:

```bash
cargo test -p rscraper-core --all-features browser -- --ignored --nocapture
```

Live provider smokes require two explicit gates and may still report a recognized
provider/layout state:

```bash
RSCRAPER_LIVE_TESTS=1 cargo test -p rscraper-cli --test live_services -- --ignored --nocapture
```

CI never receives provider secrets and does not run ignored or live tests. The
workspace contains five 0.2.0 packages and requires Rust 1.88 or newer.

## License

MIT
