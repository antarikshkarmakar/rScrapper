# Migrating from rScrapper 0.1 to 0.2

Version 0.2 replaces the temporary compatibility surface with typed requests,
explicit policy, bounded results, and shared platform services. The MSRV is Rust
1.88. Update every workspace package together; the five crates report 0.2.0.

## Fetching: `fetch` becomes `FetchClient`

The 0.1 compatibility call was:

```rust
// rScrapper 0.1
use rscraper_core::fetch::FetchMode;
use rscraper_core::{fetch, FetchOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = FetchOptions::new().mode(FetchMode::Auto);
    let page = fetch("https://example.com", &options).await?;
    println!("{}", page.html);
    Ok(())
}
```

The corresponding 0.2 call is:

```rust
// rScrapper 0.2
use rscraper_core::{FetchClient, FetchRequest};

#[tokio::main]
async fn main() -> rscraper_core::Result<()> {
    let client = FetchClient::builder().build()?;
    let request = FetchRequest::auto("https://example.com")?;
    let page = client.fetch_request(request).await?;
    println!("{} {} {}", page.status, page.url, page.html);
    Ok(())
}
```

`FetchOptions`, the free `fetch` function, and the compatibility `fetch`
module no longer exist. Use `FetchRequest::request`,
`FetchRequest::browser`, or `FetchRequest::auto`, then set typed
`headers`, `proxy`, and `host_restriction` fields as needed.

`FetchRequest::host_restriction` is new and intentionally makes any 0.1-era
external struct literal a source break. Prefer constructors so future
policy fields cannot be accidentally omitted:

```rust
use rscraper_core::{FetchHostRestriction, FetchRequest};

fn restricted_request() -> rscraper_core::Result<FetchRequest> {
    let mut request = FetchRequest::request("https://docs.rs/")?;
    request.host_restriction = Some(FetchHostRestriction::https_label_suffixes([
        "docs.rs",
    ])?);
    Ok(request)
}
```

## Results, policy, limits, proxies, and browsers

`Page.url` is now `url::Url`, `Page.via` is `FetchVia`, and decoded
document responses include `content_type`. `fetch_raw_request` returns a
`RawResponse` with bounded bytes and numeric rate-limit metadata:

```rust
use rscraper_core::{FetchClient, FetchRequest};

async fn fetch_bytes() -> rscraper_core::Result<Vec<u8>> {
    let client = FetchClient::builder().build()?;
    let raw = client
        .fetch_raw_request(FetchRequest::request("https://example.com/data.json")?)
        .await?;
    Ok(raw.bytes)
}
```

`NetworkPolicy::PublicInternet` is the default. It permits credential-free
public HTTP(S) destinations, checks all DNS answers, and revalidates redirect
hops. `NetworkPolicy::AllowPrivate` is an explicit trusted-fixture/local
diagnostic choice:

```rust
use rscraper_core::{FetchClient, NetworkPolicy, OperationLimits};
use std::time::Duration;

fn local_fixture_client() -> rscraper_core::Result<FetchClient> {
    FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(OperationLimits {
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            ..OperationLimits::default()
        })
        .build()
}
```

Proxy values are parsed `Url`s and validated before I/O. Public-policy domain
requests reject proxy/DNS combinations that cannot enforce destination policy.
TLS uses rustls and keeps certificate/hostname verification enabled.

Explicit Browser mode requires a configured renderer backed by an installed
supported Chromium/Chrome executable. Auto mode does not require a browser: it
fetches HTTP first, optionally renders an eligible JavaScript shell when a
renderer is configured, and returns the original HTTP page if no renderer is
available or rendering fails. The default application context treats failed
browser discovery as no renderer, so Auto remains HTTP-only. Register a
discovered renderer explicitly when browser rendering is wanted:

```rust
use rscraper_core::{
    BrowserBackend, BrowserEgress, BrowserRenderer, FetchClient, NetworkPolicy,
};
use std::sync::Arc;

fn browser_client() -> rscraper_core::Result<FetchClient> {
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::PublicInternet))?;
    let backend: Arc<dyn BrowserBackend> = Arc::new(renderer);
    FetchClient::builder().browser(backend).build()
}
```

Discovery searches supported executable names. Rendering keeps the OS sandbox,
uses an owned temporary profile and policy proxy, disables downloads, and
cleans its child/tasks/connections/profile on all terminal paths. Tor rendering
uses `BrowserEgress::TorRequired`; it does not retry directly.

## Selectors and Markdown

`Sel::select` is fallible. Propagate or handle its result instead of assuming
an invalid CSS/XPath expression returns an empty set:

```rust
use rscraper_core::Sel;
use scraper::Html;

fn headings(html: &str) -> rscraper_core::Result<Vec<String>> {
    let document = Html::parse_document(html);
    Sel::parse("main h2")?
        .select(&document)?
        .into_iter()
        .map(|element| Ok(element.text().collect::<String>()))
        .collect()
}
```

`html_to_markdown` remains a convenience that returns an empty string on a
conversion failure. Security- or limit-sensitive callers should use the
fallible bounded form:

```rust
use rscraper_core::markdown::{html_to_markdown_with_options, MarkdownOptions};

fn bounded_markdown(html: &str) -> rscraper_core::Result<String> {
    html_to_markdown_with_options(
        html,
        &MarkdownOptions {
            base_url: None,
            max_chars: 20_000,
        },
    )
}
```

The Markdown renderer enforces scalar and DOM-depth bounds and neutralizes
unsafe link destinations. Platform parsers do not execute ECMAScript. YouTube's
bounded lexical scanner recognizes the tested assignment, comment, regular
expression, template, and legacy-wrapper forms before extracting JSON; an
unsupported or unbalanced script layout fails conservatively. Other embedded
state must match its expected strict JSON/DOM shape. Layout drift returns a
parse or `UpstreamLayout` error instead of evaluating remote script.

## Typed crawler streams

Free-form crawl helpers are replaced by a `Crawler` built around one
`FetchClient`. `stream` validates synchronously, starts one bounded
scheduler, and returns a stream, cancellation/pause control, and shared stats:

```rust
use futures_util::StreamExt;
use rscraper_core::{CrawlConfig, Crawler, FetchClient};
use std::time::Duration;
use url::Url;

#[tokio::main]
async fn main() -> rscraper_core::Result<()> {
    let client = FetchClient::builder().build()?;
    let crawler = Crawler::new(client);
    let config = CrawlConfig {
        start_url: Url::parse("https://example.com/").expect("static URL"),
        max_pages: 20,
        concurrency: 4,
        same_origin_only: true,
        include_subdomains: false,
        respect_robots: true,
        minimum_delay: Duration::ZERO,
        proxies: Vec::new(),
    };
    let (mut pages, control, stats) = crawler.stream(config)?;
    while let Some(page) = pages.next().await {
        println!("{}", page?.url);
    }
    control.cancel();
    eprintln!("{}", stats.summary());
    Ok(())
}
```

The stream owns cancellation: dropping it cancels scheduler work. Pausing stops
new starts without cancelling in-flight work. A robots denial on an initial page
attempt is emitted as `Error::RobotsDenied`; a robots fetch HTTP failure is
emitted as `Error::HttpStatus`. Links discovered later that robots disallows
are counted/skipped rather than each producing an output error.

## CLI services and cookies

Shared CLI/library consumers should create `rscraper_cli::context::AppContext`
and call typed modules such as `web`, `youtube`, `github`, `rss`, or
`social`. `AppContext::try_default` keeps public-network policy and attaches
a browser only when discovery succeeds. `try_diagnostic` is the explicit
private-loopback context for doctor fixtures.

Raw cookie-jar exposure is replaced by `PlatformCookieJar`:

```rust
use rscraper_cli::cookies::load_platform_cookies;
use std::path::Path;
use url::Url;

fn cookies() -> rscraper_core::Result<rscraper_cli::cookies::PlatformCookieJar> {
    let origin = Url::parse("https://www.reddit.com/").expect("static URL");
    load_platform_cookies(Path::new("reddit.cookies.txt"), &origin)
}
```

On Unix the loader rejects symlinks, non-regular files, and group/other
permissions; use mode `0600`. State directories are `0700`.
`PlatformCookieJar::Debug` is redacted.

## HTTP API

The API now defaults to `127.0.0.1:8787`. `PORT` changes the port while
retaining loopback; `RSCRAPER_BIND` selects a complete socket. A non-loopback
bind requires `RSCRAPER_API_TOKEN` before listen. Set
`RSCRAPER_API_MAX_CONCURRENT_OPERATIONS` to 1–32 (default 8).

Operation routes use strict JSON, stable bounded error objects, a 64 KiB request
cap, a 10 MiB response cap, nonblocking concurrency admission, 30-second
scrape/search deadlines, and a 120-second crawl deadline. `/health` is
unauthenticated; configured bearer auth covers `/scrape`, `/search`, and
`/crawl`.

## MCP SDK and stdio

The hand-written 0.1 protocol loop is replaced by official `rmcp` 3.1.4
service/tool macros. Embedders can serve the same typed service:

```rust
use rmcp::ServiceExt;
use rscraper_cli::context::AppContext;
use rscraper_mcp::{GuardedStdioTransport, RscraperMcp};

async fn serve() -> anyhow::Result<()> {
    let service = RscraperMcp::new(AppContext::try_default()?);
    service
        .serve(GuardedStdioTransport::new(
            tokio::io::stdin(),
            tokio::io::stdout(),
        ))
        .await?
        .waiting()
        .await?;
    Ok(())
}
```

The only tools are `scrape` and `search`. The guarded transport bounds each
newline-delimited inbound frame at 1 MiB, validates SDK messages and duplicate
IDs, handles cancellation, reserves stdout for protocol output, and caps
untrusted remote tool text at one million scalar values. It intentionally has
one prefetched-frame slot: a client that pipelines beyond active SDK work plus
that slot is backpressured, not accepted into an unbounded queue.

## Robin

`Hit.url` changed from text to `url::Url`; parse and validate it at the
boundary:

```rust
use robin::Hit;
use url::Url;

fn hit() -> Hit {
    Hit {
        title: "Example".into(),
        url: Url::parse("http://exampleonionaddress.onion/").expect("fixture URL"),
        snippet: "Untrusted snippet".into(),
        source: None,
        source_warning: None,
    }
}
```

`Report::to_markdown` and `Report::save` are fallible because escaping,
output/path bounds, secure create-new semantics, and filesystem identity checks
can reject work. Propagate their `robin::Result`:

```rust
use robin::Report;
use std::path::{Path, PathBuf};

fn save_report(report: &Report) -> robin::Result<PathBuf> {
    let _preview = report.to_markdown()?;
    report.save(Path::new("reports"))
}
```

Robin requires a `socks5h` proxy (default
`socks5h://127.0.0.1:9050/`) and proves the Tor transport before any provider
call. Supported providers are OpenAI, Claude, Gemini, and Ollama, configured by
the environment described in the README. It makes at most three model calls,
retains at most five sources, keeps browser rendering on the source host through
Tor, and has no direct-network recovery path.

## Release and dependency changes

- All workspace packages: `0.2.0`
- Minimum Rust: `1.88`
- `scraper`: `0.27.0`
- `ego-tree`: `0.11`
- `rmcp`: `3.1.4`
- HTTP TLS backend: rustls-only

Regenerate against the checked-in lockfile and run the full deterministic gate
from [README.md](README.md). Remove imports of `rscraper_core::fetch`,
`FetchOptions`, compatibility `Page`, and compatibility `FetchMode::Js` /
`Stealth`; those names are not deprecated aliases in 0.2.
