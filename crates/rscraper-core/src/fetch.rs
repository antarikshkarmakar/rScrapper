//! Page fetching with three escalating strategies and automatic fallback:
//! 1. `request` — plain HTTP (fast, no JS)
//! 2. `js`      — render JavaScript via a headless browser
//! 3. `stealth` — headless browser with anti-bot hardening
//!
//! The default strategy is "auto": try a fast request first, and if the page
//! looks empty or blocked, transparently fall back to the browser so callers
//! never have to configure proxies or pick modes by hand.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::time::Duration;

/// How a page should be fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchMode {
    /// Plain HTTP request. Fastest, but does not run JavaScript.
    Request,
    /// Render with a headless browser (runs JS).
    Js,
    /// Headless browser with anti-bot hardening for sites that block obvious bots.
    Stealth,
    /// Try `Request` first, then fall back to the browser if it looks empty/blocked.
    Auto,
}

impl Default for FetchMode {
    fn default() -> Self {
        FetchMode::Auto
    }
}

/// A fetched page: final URL, status, and HTML body.
#[derive(Debug, Clone)]
pub struct Page {
    pub url: String,
    pub status: u16,
    pub html: String,
    /// Which strategy actually produced this page (useful for `doctor`/debug).
    pub via: &'static str,
}

/// Options that shape a fetch.
#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub mode: FetchMode,
    /// Extra headers to send with the request.
    pub extra_headers: Vec<(String, String)>,
    /// Optional proxy URL (e.g. `http://user:pass@host:port`).
    pub proxy: Option<String>,
    /// Per-attempt timeout in seconds.
    pub timeout_secs: u64,
}

impl FetchOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn mode(mut self, m: FetchMode) -> Self {
        self.mode = m;
        self
    }
    pub fn proxy(mut self, p: impl Into<String>) -> Self {
        self.proxy = Some(p.into());
        self
    }
}

/// The default user agent. Realistic enough to avoid the most obvious bot blocks.
pub const DEFAULT_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
(KKHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Fetch a URL according to `opts`, applying the auto-fallback ladder.
pub async fn fetch(url: &str, opts: &FetchOptions) -> Result<Page> {
    match opts.mode {
        FetchMode::Request => http_fetch(url, opts).await,
        FetchMode::Js | FetchMode::Stealth => browser_fetch(url, opts, opts.mode == FetchMode::Stealth).await,
        FetchMode::Auto => {
            if let Ok(page) = http_fetch(url, opts).await {
                // Heuristic: a real content page has some text. If it's basically
                // empty or an obvious "enable JS" shell, escalate to the browser.
                if !looks_empty_or_blocked(&page.html) {
                    return Ok(page);
                }
            }
            browser_fetch(url, opts, true).await
        }
    }
}

/// Plain HTTP fetch using reqwest (rustls, no native TLS dependency needed).
async fn http_fetch(url: &str, opts: &FetchOptions) -> Result<Page> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(opts.timeout_secs.max(1)))
        .user_agent(DEFAULT_UA)
        .danger_accept_invalid_certs(true);

    if let Some(proxy) = &opts.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).context("invalid proxy URL")?);
    }

    let client = builder.build().context("failed to build HTTP client")?;

    let mut req = client.get(url);
    for (k, v) in &opts.extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req.send().await.context(format!("request failed for {url}"))?;
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let body = resp.text().await.unwrap_or_default();

    Ok(Page { url: final_url, status, html: body, via: "request" })
}

/// Render a page with a headless browser by shelling out to `chromium`/`google-chrome`.
/// This keeps the core dependency-free (no heavy Rust browser crate) while still
/// supporting JS-heavy and bot-protected sites.
async fn browser_fetch(url: &str, opts: &FetchOptions, stealth: bool) -> Result<Page> {
    let bin = find_chromium().ok_or_else(|| anyhow!("no headless browser found (install chromium or google-chrome)"))?;

    let mut args: Vec<String> = vec![
        "--headless=new".into(),
        "--disable-gpu".into(),
        "--no-sandbox".into(),
        "--hide-scrollbars".into(),
        "--virtual-time-budget=8000".into(), // let JS settle before dumping DOM
        format!("--dump-dom={url}"),
    ];

    if stealth {
        args.push(format!("--user-agent={DEFAULT_UA}"));
        args.push("--window-size=1366,900".into());
    }

    let out = tokio::time::timeout(
        Duration::from_secs(opts.timeout_secs.max(5)),
        tokio::process::Command::new(&bin).args(args).output(),
    )
    .await
    .map_err(|_| anyhow!("browser timed out rendering {url}"))?
    .context("failed to launch browser")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("browser exited with error: {}", stderr.trim()));
    }

    let html = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(Page { url: url.to_string(), status: 200, html, via: if stealth { "stealth" } else { "js" } })
}

/// Locate a usable headless browser binary on PATH.
pub fn find_chromium() -> Option<String> {
    let candidates = ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"];
    for c in candidates {
        if which(c) {
            return Some(c.to_string());
        }
    }
    None
}

fn which(bin: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if dir.join(bin).is_file() {
            return true;
        }
    }
    false
}

/// Cheap heuristic: does this HTML look like it has no real content yet?
fn looks_empty_or_blocked(html: &str) -> bool {
    let text = strip_tags(html);
    let words: usize = text.split_whitespace().count();
    if words < 20 {
        return true;
    }
    let lower = html.to_lowercase();
    const BLOCK_HINTS: [&str; 6] = [
        "enable javascript",
        "please enable js",
        "checking your browser",
        "verify you are human",
        "access denied",
        "are you a robot",
    ];
    words < 120 && BLOCK_HINTS.iter().any(|h| lower.contains(h))
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_empty_page() {
        assert!(looks_empty_or_blocked("<html><body></body></html>"));
        assert!(!looks_empty_or_blocked(&"word ".repeat(300)));
    }

    #[test]
    fn strips_tags_for_heuristic() {
        let s = strip_tags("<p>Hello <b>world</b></p>");
        assert_eq!(s, "Hello world");
    }
}
