//! Web reading and searching — no paid APIs, with built-in fallbacks.

use crate::{rss, youtube};
use anyhow::{anyhow, Context, Result};
use rscraper_core::fetch::{self, FetchMode, FetchOptions, Page};
use rscraper_core::html_to_markdown;
use scraper::{Html, Selector};
use serde_json::json;

/// Build a reqwest client with a realistic UA (shared across the CLI).
pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .danger_accept_invalid_certs(true)
        .build()?)
}

/// A single search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Read a URL and return clean Markdown.
pub async fn read(url: &str, json_out: bool) -> Result<()> {
    let page = fetch::fetch(url, &FetchOptions::new().mode(FetchMode::Auto)).await?;
    if (page.status as u16) >= 400 && page.html.trim().is_empty() {
        return Err(anyhow!("HTTP {} for {url}", page.status));
    }
    let md = html_to_markdown(&page.html);
    emit(json_out, &json!({ "url": page.url, "status": page.status, "via": page.via, "markdown": md }), &md)
}

/// Web search with DuckDuckGo primary and Bing fallback.
pub async fn search(query: &str, n: usize, scrape: bool, json_out: bool) -> Result<()> {
    let mut hits = ddg_search(query, n).await.unwrap_or_default();
    if hits.is_empty() {
        eprintln!("(DuckDuckGo returned nothing — trying Bing fallback…)");
        hits = bing_search(query, n).await.unwrap_or_default();
    }

    let mut results: Vec<serde_json::Value> = Vec::new();
    for h in &hits {
        let mut obj = json!({ "title": h.title, "url": h.url, "snippet": h.snippet });
        if scrape {
            match read_page_markdown(&h.url).await {
                Ok(md) => {
                    obj["markdown"] = json!(md);
                }
                Err(_) => {} // keep the hit even if its page can't be cleaned
            }
        }
        results.push(obj);
    }

    let text = render_hits(&hits, scrape);
    emit(json_out, &json!({ "query": query, "count": hits.len(), "results": results }), &text)
}

async fn read_page_markdown(url: &str) -> Result<String> {
    let page = fetch::fetch(url, &FetchOptions::new().mode(FetchMode::Auto)).await?;
    Ok(html_to_markdown(&page.html))
}

/// DuckDuckGo HTML endpoint (no API key).
async fn ddg_search(query: &str, n: usize) -> Result<Vec<SearchHit>> {
    let client = http_client()?;
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
    let html = client.get(&url).send().await?.text().await?;

    let doc = Html::parse_fragment(&html);
    let a_sel = Selector::parse("a.result__a").unwrap();
    let snip_sel = Selector::parse(".result__snippet").unwrap();

    let mut out = Vec::new();
    for (i, el) in doc.select(&a_sel).enumerate() {
        if i >= n {
            break;
        }
        let href = el.value().attr("href").unwrap_or_default().to_string();
        let real_url = unwrap_ddg_redirect(&href);
        let title = el.text().collect::<String>();
        // Snippet is a sibling in the same result row.
        let snippet = doc.select(&snip_sel).nth(i).map(|s| s.text().collect::<String>()).unwrap_or_default();
        out.push(SearchHit { title: clean(title), url: real_url, snippet: clean(snippet) });
    }
    Ok(out)
}

/// Bing HTML endpoint (fallback).
async fn bing_search(query: &str, n: usize) -> Result<Vec<SearchHit>> {
    let client = http_client()?;
    let url = format!("https://www.bing.com/search?q={}&count={}", urlencode(query), n);
    let html = client.get(&url).send().await?.text().await?;

    let doc = Html::parse_fragment(&html);
    let sel = Selector::parse("li.b_algo h2 a").unwrap();

    // Bing lists results and their snippets in the same order, so we pair by index.
    let snip_sel = Selector::parse(".b_caption p").unwrap();
    let snippets: Vec<String> = doc.select(&snip_sel).map(|p| clean(p.text().collect::<String>())).collect();

    let mut out = Vec::new();
    for (i, el) in doc.select(&sel).enumerate() {
        if out.len() >= n {
            break;
        }
        let href = el.value().attr("href").unwrap_or_default().to_string();
        let title = clean(el.text().collect::<String>());
        let snippet = snippets.get(i).cloned().unwrap_or_default();
        out.push(SearchHit { title, url: href, snippet });
    }
    Ok(out)
}

/// DuckDuckGo wraps result URLs in a redirect; extract the real target.
fn unwrap_ddg_redirect(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let rest = &href[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    href.to_string()
}

/// Smart router: detect what the target is and dispatch.
pub async fn smart_get(target: &str, n: usize, json_out: bool) -> Result<()> {
    let t = target.trim();

    // YouTube video?
    if let Some(id) = youtube::extract_video_id(t) {
        return youtube::subs(&id, json_out).await;
    }
    // RSS/Atom feed?
    if is_feed_url(t) || rss::looks_like_feed(t) {
        return rss::parse(t, json_out).await;
    }
    // A full URL → read it.
    if t.starts_with("http://") || t.starts_with("https://") {
        return read(t, json_out).await;
    }
    // Otherwise treat as a search query.
    search(t, n, false, json_out).await
}

fn is_feed_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("/rss") || lower.contains("/feed") || lower.ends_with(".xml") || lower.contains("atom.xml")
}

// ---- small helpers -------------------------------------------------------

pub fn clean(s: String) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn render_hits(hits: &[SearchHit], scraped: bool) -> String {
    let mut s = String::new();
    if hits.is_empty() {
        return "No results.".to_string();
    }
    for (i, h) in hits.iter().enumerate() {
        s.push_str(&format!("{}. {}\n   {}\n", i + 1, h.title, h.url));
        if !h.snippet.is_empty() {
            s.push_str(&format!("   {}\n", h.snippet));
        }
        if scraped {
            s.push('\n');
        }
    }
    s.trim_end().to_string()
}

/// Print JSON (for agents) or plain text.
pub fn emit(json_out: bool, json_val: &serde_json::Value, text: &str) -> Result<()> {
    if json_out {
        println!("{}", serde_json::to_string_pretty(json_val).context("serialize")?);
    } else {
        println!("{text}");
    }
    Ok(())
}
