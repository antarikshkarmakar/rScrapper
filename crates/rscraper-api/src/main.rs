//! rScrapper API — self-hosted HTTP service that turns any URL into LLM-ready text.
//!
//! Endpoints:
//! * `GET  /health`   → liveness
//! * `POST /scrape`   → `{ "url": ... }` → clean Markdown
//! * `POST /search`   → `{ "query": ..., "n": .., "scrape": bool }` → results (+cleaned pages)
//! * `POST /crawl`    → `{ "start_url": ..., "max_pages": .. }` → all pages as Markdown

use anyhow::Result;
use rscraper_core::fetch::{self, FetchMode, FetchOptions};
use serde_json::json;
use std::io::{Read, Write};

const MAX_BODY: usize = 1 << 20; // 1 MiB request body cap

#[tokio::main]
async fn main() -> Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8787);
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("rScrapper API listening on http://{addr}  (endpoints: /scrape /search /crawl)");

    loop {
        if let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(handle(stream));
        }
    }
}

async fn handle(mut stream: tokio::net::TcpStream) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read the request head + body (simple HTTP/1.1).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if head_complete(&buf) && body_complete(&buf) {
                    break;
                }
                if buf.len() > MAX_BODY + 8192 {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    let req = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = match req.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => (req.clone(), String::new()),
    };

    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let method = request_line.split_whitespace().next().unwrap_or("GET").to_string();
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

    // Route.
    let (status, payload) = route(&method, &path, &body).await;

    let resp_body = if status == 200 {
        payload
    } else {
        json!({ "error": payload }).to_string()
    };
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
        reason(status),
        resp_body.len()
    );

    let _ = stream.write_all(response.as_bytes()).await;
}

async fn route(method: &str, path: &str, body: &str) -> (u16, String) {
    match (method, path.trim_end_matches('/')) {
        ("GET", "/health") => (200, json!({ "status": "ok", "service": "rscraper-api" }).to_string()),

        ("POST", "/scrape") => {
            let url = body_field(body, "url");
            match url {
                Some(u) => match fetch::fetch(&u, &FetchOptions::new().mode(FetchMode::Auto)).await {
                    Ok(page) => (200, json!({ "url": page.url, "status": page.status, "markdown": rscraper_core::html_to_markdown(&page.html) }).to_string()),
                    Err(e) => (502, e.to_string()),
                },
                None => (400, "missing `url`".into()),
            }
        }

        ("POST", "/search") => {
            let query = body_field(body, "query");
            match query {
                Some(q) => search_route(&q, body).await,
                None => (400, "missing `query`".into()),
            }
        }

        ("POST", "/crawl") => {
            let start = body_field(body, "start_url").or_else(|| body_field(body, "url"));
            match start {
                Some(u) => crawl_route(&u, body).await,
                None => (400, "missing `start_url`".into()),
            }
        }

        _ => (404, format!("no route for {method} {path}")),
    }
}

/// Search via DuckDuckGo HTML endpoint (+ optional per-result scrape).
async fn search_route(query: &str, body: &str) -> (u16, String) {
    let n = body_num(body, "n").unwrap_or(5);
    let scrape = body_field(body, "scrape").map(|s| s == "true" || s == "1").unwrap_or(false);

    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) Chrome/124.0.0.0 Safari/537.36")
        .build()
    {
        Ok(c) => c,
        Err(e) => return (500, e.to_string()),
    };

    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
    match client.get(&url).send().await {
        Ok(resp) => match resp.text().await {
            Ok(html) => {
                let hits = parse_ddg(&html, n);
                let mut results: Vec<serde_json::Value> = Vec::new();
                for h in &hits {
                    let mut obj = json!({ "title": h.0, "url": h.1, "snippet": h.2 });
                    if scrape {
                        if let Ok(page) = fetch::fetch(&h.1, &FetchOptions::new().mode(FetchMode::Auto)).await {
                            obj["markdown"] = json!(rscraper_core::html_to_markdown(&page.html));
                        }
                    }
                    results.push(obj);
                }
                (200, json!({ "query": query, "count": hits.len(), "results": results }).to_string())
            }
            Err(e) => (502, e.to_string()),
        },
        Err(e) => (502, e.to_string()),
    }
}

/// Crawl a site with the core spider and return each page as Markdown.
async fn crawl_route(start: &str, body: &str) -> (u16, String) {
    let max_pages = body_num(body, "max_pages").unwrap_or(20);
    let concurrency = body_num(body, "concurrency").unwrap_or(4);

    use futures_util::StreamExt;
    use rscraper_core::spider::{crawl_stream, SpiderConfig};

    let config = SpiderConfig { start_url: start.to_string(), max_pages, concurrency, ..Default::default() };

    // Real fetcher closure.
    let (stream, _state) = crawl_stream(config, |u| {
        Box::pin(async move { fetch::fetch(&u, &FetchOptions::new().mode(FetchMode::Auto)).await })
    });

    let results: Vec<_> = stream.collect().await;
    let mut pages: Vec<serde_json::Value> = Vec::new();
    for r in results {
        if let Ok(r) = r {
            pages.push(json!({ "url": r.url, "status": r.status, "markdown": rscraper_core::html_to_markdown(&r.html) }));
        }
    }

    (200, json!({ "start_url": start, "count": pages.len(), "pages": pages }).to_string())
}

// ---- helpers -------------------------------------------------------------

fn parse_ddg(html: &str, n: usize) -> Vec<(String, String, String)> {
    let doc = scraper::Html::parse_fragment(html);
    let a_sel = scraper::Selector::parse("a.result__a").unwrap();
    let snip_sel = scraper::Selector::parse(".result__snippet").unwrap();
    let mut out = Vec::new();
    for (i, el) in doc.select(&a_sel).enumerate() {
        if i >= n {
            break;
        }
        let href = el.value().attr("href").unwrap_or_default();
        let real = unwrap_ddg(href);
        let title: String = el.text().collect::<String>();
        let snippet: String = doc.select(&snip_sel).nth(i).map(|s| s.text().collect()).unwrap_or_default();
        out.push((title, real, snippet));
    }
    out
}

fn unwrap_ddg(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let rest = &href[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    href.to_string()
}

fn body_field(body: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| v.get(key).cloned()).and_then(|v| match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

fn body_num(body: &str, key: &str) -> Option<usize> {
    serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| v.get(key)?.as_u64()).map(|n| n as usize)
}

fn head_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

fn body_complete(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    let head_end = match text.find("\r\n\r\n") {
        Some(i) => i,
        None => return false,
    };
    let content_length = text[..head_end]
        .lines()
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length:")?
                .trim()
                .parse::<usize>()
                .ok()
        })
        .unwrap_or(0);
    if content_length == 0 {
        return true; // no body expected (GET / HEAD)
    }
    let body_len = text.len() - head_end - 4;
    body_len >= content_length
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "OK",
    }
}

fn urlencode(s: &str) -> String {
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
