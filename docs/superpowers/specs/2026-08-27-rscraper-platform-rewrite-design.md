# rScrapper Platform Rewrite Design

**Status:** Implemented through the 0.2.0 release checkpoint; controller review
and history consolidation remain

**Date:** 2026-08-27

**Implementation checkpoint:** 2026-08-30
**Target:** rScrapper 0.2.0  
**Audience:** Maintainers and contributors implementing or reviewing the rewrite

## Summary

rScrapper 0.2.0 replaces the previous best-effort internals with a secure,
bounded, testable scraping platform while preserving the documented CLI
commands, HTTP routes, MCP tool names, and JSON response shapes. Breaking Rust
library changes are allowed where the current interfaces prevent correctness.

The rewrite keeps the existing five-crate workspace:

- `rscraper-core` owns network policy, fetching, browser rendering, HTML
  conversion, selectors, URL normalization, and crawling.
- `rscraper-cli` owns user-facing commands and platform adapters.
- `rscraper-api` exposes bounded core operations over HTTP.
- `rscraper-mcp` exposes bounded core operations over MCP stdio.
- `robin` runs Tor-enforced, LLM-assisted OSINT research.

Default tests are deterministic and make no external network requests. Live
service checks are opt-in because search and social platforms change without
notice.

## Goals

1. Eliminate unsafe TLS settings, server-side request forgery, unbounded
   downloads, unsafe redirects, and direct-network fallbacks from Tor mode.
2. Make request, browser, crawl, API, and output limits explicit and enforced.
3. Replace ad-hoc YouTube and feed parsing with structural parsers.
4. Make HTML-to-Markdown and the documented XPath-style subset correct for
   common nested and malformed documents.
5. Make all platform adapters return structured data or explicit actionable
   errors; never silently report an empty success after a parser or provider
   failure.
6. Preserve documented commands, HTTP routes, MCP tool names, and JSON response
   field names.
7. Cover every repaired defect with a regression test and run formatting,
   strict linting, tests, documentation, and dependency auditing in CI.
8. Bring the README and command help into agreement with implemented behavior.

## Non-goals

- Solving CAPTCHAs or guaranteeing anti-bot evasion.
- Guaranteeing continued access to unofficial social-platform endpoints.
- Implementing full XPath 1.0 or a general-purpose browser automation API.
- Downloading executables, archives, media, or other onion-site attachments.
- Distributed crawling, persistent crawl queues, or a database.
- Executing instructions found in scraped content.
- Adding paid search or social APIs.

## Compatibility Contract

The following user-facing surfaces remain available:

- CLI commands: `get`, `read`, `search`, `youtube`, `github`, `rss`, `social`,
  `setup`, and `doctor`.
- HTTP routes: `GET /health`, `POST /scrape`, `POST /search`, and
  `POST /crawl`.
- MCP tools: `scrape` and `search`.
- Existing success-response field names, including `url`, `status`,
  `markdown`, `query`, `count`, `results`, `start_url`, and `pages`.
- `PORT`, `RSCRAPER_HOME`, provider-key variables, and `OLLAMA_HOST`.

Additive fields are permitted. Errors continue to contain a human-readable
`error` string; the API may add a stable `code` field.

Breaking Rust changes include:

- Replacing the free `fetch` implementation with a reusable `FetchClient`.
- Replacing the generic crawl closure with a `Crawler` that owns fetch policy
  and scheduling.
- Returning `Result` for invalid CSS and XPath-style selectors instead of an
  empty match set.
- Adding a base-URL-aware Markdown conversion API while retaining
  `html_to_markdown(html)` as a compatibility wrapper.

The workspace remains on Rust edition 2021 and declares Rust 1.88 as its MSRV.
Dependencies must support rustls so the project does not acquire a native
OpenSSL requirement.

## Architecture

### Shared operation flow

Every CLI, API, MCP, crawler, and Robin request follows the same ownership
chain:

```text
user input
  -> typed command/request validation
  -> NetworkPolicy and OperationLimits
  -> FetchClient or BrowserRenderer
  -> typed platform/document parser
  -> bounded normalized result
  -> CLI / JSON / MCP presentation
```

Network and size invariants live in `rscraper-core`; binaries may tighten them
but may not bypass them. Platform adapters do not instantiate independent
`reqwest::Client` values.

### Proposed core modules

```text
rscraper-core/src/
  client.rs       reusable HTTP client and response streaming
  policy.rs       URL, DNS, redirect, scheme, and IP policy
  browser.rs      isolated Chromium lifecycle and request interception
  document.rs     fetched page metadata and content classification
  markdown.rs     DOM-aware HTML-to-Markdown renderer
  selectors.rs    CSS and XPath-style selection
  urlnorm.rs      crawl URL resolution and canonicalization
  robots.rs       robots.txt parsing, caching, and crawl delay
  spider.rs       bounded concurrent frontier and crawl state
```

Existing file names may be retained where doing so keeps the diff clearer, but
each responsibility above must have one owner.

## Network Security and Fetching

### Network policy

`NetworkPolicy` has two explicit modes:

- `PublicInternet` is the default for every binary. It permits only `http` and
  `https`, rejects URL credentials, and rejects destinations resolving to
  loopback, private, link-local, multicast, unspecified, documentation,
  benchmarking, carrier-grade NAT, and other non-public address ranges for
  IPv4 and IPv6.
- `AllowPrivate` is an explicit library/CLI opt-in for local development. The
  HTTP API never enables it from an untrusted request.

A custom DNS resolver enforces the policy at connection resolution time, not
only in a preflight check. Literal IP hosts pass through the same classifier.
All redirect targets are revalidated, and the redirect limit is ten. A DNS
answer containing any forbidden address is rejected rather than selecting one
of its public answers.

Only HTTP(S) documents are fetchable. `file:`, `data:`, `ftp:`, browser-internal
schemes, and embedded credentials are rejected before either request or
browser mode runs.

### Fetch client

`FetchClient` owns a shared `reqwest::Client`, network policy, cookie scope, and
operation limits. Defaults are:

- connect timeout: 10 seconds;
- complete request timeout: 30 seconds;
- redirect limit: 10;
- maximum decoded response body: 5 MiB, enforced while streaming;
- accepted document types: HTML, XHTML, XML, JSON, and text;
- TLS certificate and hostname verification enabled;
- decompression handled by the HTTP client;
- one connection pool reused for an operation or process.

Response bodies are streamed and stopped at the byte limit. Invalid text is
decoded lossily only after charset-aware decoding has been attempted. A
non-success HTTP status remains part of `Page` and is not silently turned into
success. Parser and API callers decide which statuses are acceptable.

Cookies use a distinct jar per platform session. A cookie supplied for one
platform cannot be sent after a cross-origin redirect. Authenticated platform
requests do not permit invalid certificates or public proxies containing
credentials in diagnostic output.

### Browser renderer

Browser rendering uses a maintained Chrome DevTools Protocol client instead of
constructing an ambiguous `--dump-dom=<url>` command. Each render:

- uses a fresh temporary profile;
- keeps Chromium's OS sandbox enabled;
- disables downloads, extensions, background networking, and unnecessary
  media requests;
- applies the configured HTTP or SOCKS proxy;
- intercepts every request and applies `NetworkPolicy` before allowing it;
- limits navigation to 30 seconds and DOM output to 5 MiB;
- records the final main-document URL and best available main-document status;
- kills and reaps the browser on timeout, cancellation, or error;
- removes the temporary profile on completion.

`Auto` mode tries HTTP first. It renders only when the HTTP response is a
successful HTML document that matches the tested empty/JavaScript-shell
heuristic, or when an explicitly configured status is eligible for rendering.
It preserves the original HTTP error when rendering cannot improve it.

Tor mode passes a SOCKS proxy to both HTTP and browser transports and rejects
any transport that cannot prove the proxy is active. It never retries directly.

## HTTP API

The custom socket-level HTTP parser is replaced by Axum and Tower middleware.
The API defaults to `127.0.0.1:$PORT`; `RSCRAPER_BIND` may select another
address. Binding a non-loopback address requires `RSCRAPER_API_TOKEN` at
startup. Public requests authenticate with `Authorization: Bearer <token>`.

Defaults and hard maxima are:

| Limit | Default | Maximum |
|---|---:|---:|
| JSON request body | n/a | 64 KiB |
| route duration | 30 s | 120 s for crawl |
| concurrent operations | 8 | 32 configured at startup |
| search results | 5 | 20 |
| crawl pages | 20 | 100 |
| crawl concurrency | 4 | 16 |
| total JSON response | n/a | 10 MiB |

The API rejects unsupported content types, malformed JSON, out-of-range values,
unsafe URLs, and oversized outputs with appropriate 4xx responses. Upstream
status and timeout errors use 502 and 504. It does not enable wildcard CORS.
Health checks do not perform external network access.

## Crawler

`Crawler` owns one central frontier, a queued set, a completed set, a
`FetchClient`, and an in-flight future set. Its behavior is deterministic:

1. Normalize and validate the start URL.
2. Check and cache robots.txt for its origin when `respect_robots` is enabled.
3. Start at most `concurrency` fetches.
4. Count an attempt before scheduling it; never schedule more than
   `max_pages`, whether prior requests succeeded or failed.
5. Resolve relative links against the fetched page's final URL.
6. Strip fragments, normalize scheme/host casing and default ports, preserve
   path/query semantics, and reject unsupported schemes.
7. Deduplicate before enqueueing.
8. Follow only HTML/XHTML documents and only the configured origin by default.
9. Respect robots exclusions and crawl delay, with a configured minimum delay
   taking precedence when larger.
10. Stop on cancellation and close the result stream after all in-flight work
    is reaped.

`same_domain_only` becomes `same_origin_only` because scheme, host, and port
jointly define the default boundary. An explicit broader policy can permit
subdomains. Links that look destructive (`logout`, `signout`, `delete`,
`remove`, or equivalent action query keys) are not followed by default.

Stats distinguish `attempted`, `succeeded`, `failed`, `queued`, and `skipped`.
Pause/resume uses notification rather than a polling sleep.

## HTML-to-Markdown

The renderer walks DOM nodes rather than collecting descendant text at block
boundaries. It supports:

- headings, paragraphs, line breaks, emphasis, strong text, deletion, links,
  images with alt text, inline code, fenced code, blockquotes, thematic breaks,
  nested ordered/unordered lists, description lists, and tables;
- links and images resolved against an optional base URL;
- Markdown escaping for text, link labels, destinations, and table cells;
- dynamically sized code fences when source code contains backticks;
- `thead`, `tbody`, `tfoot`, missing table sections, and uneven row widths;
- direct text nodes mixed with child elements;
- exact or boundary-aware non-content class matching rather than arbitrary
  substrings such as `ad` inside `reading`;
- explicit output character limits.

Scripts, styles, templates, hidden nodes, and browser chrome are skipped.
Forms are omitted except for meaningful labels and static text. The content
root preference remains `main`, `[role=main]`, `article`, then `body`, but a
candidate with no meaningful text does not hide a better fallback.

The compatibility function `html_to_markdown(html)` uses no base URL and
default output limits. `html_to_markdown_with_base(html, base, limits)` is the
canonical API.

## Selectors

CSS parsing returns an error for invalid selectors. The XPath-style subset is
explicitly not full XPath and supports:

- `/` child steps and `//` descendant steps;
- element names and `*`;
- `[@attr]` and `[@attr='value']`;
- `[contains(@attr,'text')]`;
- one-based `[n]`, applied to the ordered candidates produced by that step;
- multiple predicates combined with logical AND.

The parser respects brackets and quoted `/`, `]`, commas, and parentheses.
Malformed expressions return errors. Results are document ordered and
deduplicated. The selector-memory fallback keeps tag, normalized text, stable
attributes, and class tokens; it requires a configurable minimum score and
never treats substring class matches as exact class matches.

## Feeds

RSS and Atom use a structural XML/feed parser. The normalized item shape stays:

```json
{
  "title": "...",
  "link": "https://...",
  "description": "...",
  "date": "..."
}
```

The adapter handles namespaces, CDATA, escaped entities, Atom alternate links,
RSS GUID fallbacks, relative links against the feed URL, common date formats,
multiline elements, and individual malformed entries. One malformed entry does
not discard valid siblings. The default and maximum item count are 20 and 100.
Descriptions are converted from HTML to bounded plain text. XML external
entities and DTD network access are disabled.

## YouTube

The adapter extracts `ytInitialPlayerResponse` or the equivalent player JSON
using a quote-aware balanced JSON scanner, then deserializes only required
fields. Caption selection follows:

1. user-requested language when supplied;
2. exact English track;
3. another human-created track;
4. an automatically generated track;
5. the first available track.

Caption URLs must remain HTTPS and on an allowed YouTube/Google host. JSON3 is
preferred and XML is the fallback. Parsers join segment text, decode entities,
preserve paragraph boundaries where timestamps indicate a pause, and ignore
formatting-only events. Video IDs accept the documented eleven-character
alphabet including `-` and `_`.

Search parsing deserializes the embedded JSON rather than scanning fixed-size
windows. Layout or consent pages produce explicit errors containing no cookie
values or page body.

## Search, GitHub, and Social Adapters

Each adapter has typed response models, a pure parser function, stored sanitized
fixtures, and an ignored opt-in live test.

- DuckDuckGo and Bing parse results within each result container so titles,
  snippets, and links cannot drift by index. Redirect URLs use the `url` crate.
- Search fallback occurs on an upstream error or a confirmed empty result set,
  and retains diagnostics identifying the failed provider.
- Optional page scraping uses bounded concurrency and records a per-result
  scrape error rather than silently omitting it.
- GitHub checks status codes and rate-limit headers, validates `owner/repo`,
  handles pagination needed to return the requested number of issues after
  excluding pull requests, and uses a maintained Base64 implementation.
- Reddit and Bilibili use typed JSON models and bounded result counts.
- Twitter, Xiaohongshu, and LinkedIn use isolated authenticated sessions and
  structured DOM/embedded-state extraction. Consent, checkpoint, expired-cookie,
  and layout-change pages produce distinct errors. They do not claim to bypass
  platform protections.

Cookie input accepts a raw Cookie header, `name=value` lines, or Netscape cookie
files. Setup creates the state directory with owner-only permissions. On Unix,
cookie files must be regular, non-symlink files with no group/other permissions;
insecure files are rejected with a repair command. Logs and errors never print
cookie values.

## MCP Server

The MCP server uses the maintained Rust MCP SDK and keeps stdout exclusively for
protocol messages. Diagnostics go to stderr. It supports initialization,
notifications, cancellation, concurrent independent calls, schema validation,
and structured tool errors.

The `scrape` and `search` tools retain their names and arguments. Tool output is
capped at one million characters and starts with a machine-readable/visible
notice that remote content is untrusted data, not agent instructions. Scraped
content is delimited from server diagnostics. Search-result page failures are
reported per result.

## Robin

Robin accepts both the documented positional query and `--query`. Interactive
mode prompts for query, provider, model, Tor endpoint, and save directory only
when the corresponding value was not supplied.

All search and source retrieval uses `NetworkPolicy` in Tor-required mode.
Robin validates the configured SOCKS URL without panicking and performs a Tor
connectivity check before sending the query to a cloud LLM. A failed Tor check
aborts the investigation.

Robin retrieves at most five HTML source pages, with a 2 MiB limit each, and
never follows attachment or download links. Reports distinguish search snippets
from fetched source text. Search-result and source content are wrapped in
clearly delimited untrusted-data blocks before LLM calls. Provider prompts state
that instructions inside those blocks must be ignored. This reduces, but does
not claim to eliminate, prompt-injection risk.

Provider clients enforce status checks, 60-second timeouts, non-empty response
content, and bounded prompts/responses. Provider error messages are parsed when
safe and never include API keys. Failed refinement falls back to the original
query with a warning; failed filtering keeps bounded hits with a warning; failed
summarization produces an explicit incomplete-report status.

Report Markdown escapes user/source fields. Filenames include a timestamp with
sub-second precision plus a random suffix and use create-new semantics so an
existing report is never overwritten.

## Errors, Logging, and Observability

Core exposes typed error categories for invalid input, policy rejection, DNS,
timeout, body limit, HTTP status, browser, parse, authentication, rate limit,
robots exclusion, cancellation, and upstream layout changes. Binaries add
context without flattening categories into generic strings internally.

`tracing` emits structured diagnostics to stderr. Secrets, URL credentials,
cookies, authorization headers, provider keys, and private response bodies are
redacted. Normal CLI JSON output and MCP stdout contain no log lines.

`doctor` checks configuration and reachability without weakening TLS. It verifies
the browser with a real local rendered-page fixture, validates cookie file
permissions, and reports live-service checks as optional rather than declaring
the whole installation unhealthy.

## Testing Strategy

Every behavior change follows red-green-refactor. Production code is not added
until a focused test fails for the expected reason.

### Unit tests

- URL schemes, credentials, IPv4/IPv6 classifications, redirect targets, and
  URL normalization.
- byte limits, status propagation, charset handling, and render heuristics.
- Markdown escaping, mixed nodes, nested lists, tables, code fences, content
  roots, and false-positive skip classes.
- CSS errors and every supported XPath-style construct, including malformed
  input.
- RSS/Atom namespaces, CDATA, entities, dates, alternate/relative links, and
  malformed entries.
- YouTube balanced JSON extraction, JSON3/XML captions, language selection,
  escaped titles, and `-`/`_` video IDs.
- Search, GitHub, Reddit, Bilibili, and authenticated social fixtures.
- Cookie formats and permission validation.
- Robin provider parsing, prompt delimiters, escaping, and collision-safe save.

### Local integration tests

A local test server covers redirects to forbidden addresses, redirect loops,
slow headers/bodies, chunked bodies, oversized compressed and decompressed
responses, 404/429/500 statuses, incorrect content types, relative links,
robots.txt, crawl traps, cancellations, and concurrency bounds.

API tests cover authentication, loopback/public startup rules, content type,
malformed JSON, every parameter bound, timeout mapping, output caps, and all
success response shapes. CLI tests cover documented examples without external
network access. MCP tests cover initialization, list/call, invalid arguments,
cancellation, concurrent calls, stdout framing, and untrusted-content notices.

Browser integration uses a local HTML/JavaScript fixture and is skipped with a
clear reason when no supported browser is installed.

### Live tests

Live tests are marked ignored and require `RSCRAPER_LIVE_TESTS=1`. Authenticated
tests additionally require their platform cookie file. They assert only durable
invariants such as a successful status and structurally valid result; fixture
tests own exact parsing assertions.

## CI and Quality Gates

CI runs on Linux with exact Rust 1.88.0 and current stable Rust:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test --workspace --all-targets --all-features`
4. `cargo test --workspace --doc --all-features`
5. executable README contract checks
6. `cargo audit --deny warnings` with cargo-audit 0.22.2
7. an all-feature release build

The committed lockfile may not contain known vulnerabilities. Yanked or
unmaintained transitive dependencies require a documented exception and an
upgrade/removal issue. CI does not run live service tests or require secrets.

## Documentation Deliverables

The rewrite updates:

- README quick start, commands, API/MCP/Robin examples, limitations, and privacy
  claims;
- environment-variable and configuration reference;
- `SECURITY.md` with the SSRF model, safe deployment, cookie handling, Tor
  guarantees, prompt-injection warning, and vulnerability reporting;
- `MIGRATION.md` describing 0.1-to-0.2 Rust API changes and server binding/auth;
- CLI help and `doctor` messages;
- rustdoc for all public core types and policy defaults.

Documentation must not claim CAPTCHA bypass, universal social support, static
binaries, or guaranteed clean/safe LLM content.

## Delivery Sequence

The implementation plan will divide work into independently testable slices in
this order:

1. quality baseline and shared error/limit types;
2. secure HTTP policy and fetch client;
3. browser lifecycle and Tor enforcement;
4. Markdown and selector parsers;
5. feed and YouTube parsers;
6. crawler scheduler, robots, and URL normalization;
7. search, GitHub, and social adapters;
8. Axum API;
9. MCP SDK migration;
10. Robin pipeline;
11. Robin, browser lifecycle, and end-to-end product integration;
12. release identity, dependency alignment, documentation, CI, audit, and
    release gates.

Each slice begins with failing tests and ends with the full workspace test suite
passing. Task 12 leaves one reviewable release checkpoint without rewriting
earlier history. The controller owns independent approval and the requested
final history consolidation.

## Final implementation decisions

- All five packages inherit version 0.2.0 and Rust 1.88. The locked parser stack
  is `scraper = 0.27.0` with `ego-tree = 0.11`; the stale `fxhash 0.2.1`
  path and yanked `chacha20 0.10.1` lock entry were removed.
- The temporary 0.1 `fetch`/`FetchOptions` facade is removed. Typed
  `FetchClient`/`FetchRequest`, fallible selectors, bounded Markdown,
  `Crawler` streams, `AppContext`, and `PlatformCookieJar` are the 0.2
  public contracts.
- MCP uses rmcp 3.1.4. Its guarded stdio transport has a deliberate single
  prefetched-frame slot and backpressures additional pipelining; this is a
  bounded limitation, not an unbounded request queue.
- Browser cleanup completion is proved through owned lifecycle state: child
  reaped, controller tasks zero, profile removed, proxy listener closed, owned
  connections drained, and terminal state set. A former address-connect
  assertion could observe an unrelated listener after ephemeral-port reuse and
  was replaced with this ownership proof.
- README commands are extracted from an explicit marker block, strictly
  allowlisted, and executed with Cargo offline. SECURITY and migration documents
  describe the implemented boundaries and source breaks rather than aspirational
  guarantees.
- CI uses immutable action revisions, least-privilege permissions, no secrets or
  live tests, stable and exact-MSRV jobs, cargo-audit 0.22.2, and the locked
  release graph. The Task 12 report records the fresh gate outputs and counts.

## Acceptance Criteria

The rewrite is complete only when all of the following are demonstrated by
fresh command output:

- All default tests pass without external network access or secrets.
- Formatting and strict Clippy pass with no warnings.
- The API cannot fetch forbidden IP ranges directly or through redirects and
  cannot bind publicly without a token.
- TLS verification is enabled for every authenticated and unauthenticated
  client.
- Browser mode renders the requested local JavaScript fixture, uses the selected
  proxy, and leaves no child process or profile behind after timeout.
- Tor-required tests prove no direct fallback path is invoked.
- Crawl attempts never exceed `max_pages`; concurrency, deduplication, relative
  resolution, robots, pause/resume, and cancellation tests pass.
- Parser fixtures cover the behaviors listed in this specification.
- README CLI examples parse successfully; network-dependent examples are clearly
  labeled.
- MCP protocol integration tests pass without non-protocol stdout.
- `cargo audit` reports no known vulnerabilities; any allowed warning is
  documented.
- `git status --short` is empty after the final commit.
