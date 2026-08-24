//! Robin — AI-powered dark web OSINT research (inspired by NetworkChuck's guide).
//!
//! Pipeline:  user query → LLM refines it → search .onion engines over Tor →
//! LLM filters the most relevant hits → LLM writes a clean summary → save to file.
//!
//! * **Tor**: routes through `socks5://127.0.0.1:9050` (or `tor --socks-port`).
//! * **LLM providers**: OpenAI, Claude (Anthropic), Gemini, or local Ollama — chosen at runtime.
//! * **Captcha / Cloudflare fallback**: if a plain request is blocked, retry with a
//!   headless browser (`chromium`) to get past hCaptcha/reCAPTCHA/Cloudflare.
//! * **Privacy**: everything runs locally; only your query + results touch the LLM you pick.

use anyhow::{anyhow, Context, Result};
use rscraper_core::fetch::{self, FetchMode, FetchOptions};
use serde_json::json;
use std::time::Duration;

/// Which LLM to use for refine/filter/summarize.
#[derive(Debug, Clone)]
pub enum Provider {
    OpenAI { model: String },
    Claude { model: String },
    Gemini { model: String },
    Ollama { model: String },
}

impl Default for Provider {
    fn default() -> Self {
        Provider::Ollama { model: "llama3".into() }
    }
}

/// A single dark web search hit.
#[derive(Debug, Clone)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// The full investigation result (ready to save).
#[derive(Debug, Clone)]
pub struct Report {
    pub original_query: String,
    pub refined_query: String,
    pub hits: Vec<Hit>,
    pub summary: String,
}

impl Report {
    /// Render the report as a clean Markdown document.
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str("# Robin — Dark Web Investigation\n\n");
        s.push_str(&format!("- **Original query:** {}\n", self.original_query));
        s.push_str(&format!("- **Refined query:** {}\n", self.refined_query));
        s.push_str(&format!("- **Hits found:** {}\n\n", self.hits.len()));

        if !self.hits.is_empty() {
            s.push_str("## Relevant sources\n\n");
            for (i, h) in self.hits.iter().enumerate() {
                s.push_str(&format!("{}. [{}]({})\n   {}\n\n", i + 1, h.title, h.url, h.snippet));
            }
        }

        s.push_str("## Summary of findings\n\n");
        s.push_str(self.summary.trim());
        s.push('\n');
        s
    }

    /// Save to a file (creates parent dirs). Returns the path.
    pub fn save(&self, dir: &str) -> Result<std::path::PathBuf> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = std::path::Path::new(dir).join(format!("robin-report-{ts}.md"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, self.to_markdown())?;
        Ok(path)
    }
}

/// Run the full investigation pipeline.
pub async fn investigate(query: &str, provider: Provider, tor_socks: Option<String>) -> Result<Report> {
    // 1) Refine the query with the LLM for better dark web search terms.
    let refined = refine_query(&provider, query).await.unwrap_or_else(|_| query.to_string());

    // 2) Search .onion engines over Tor (with headless-browser captcha fallback).
    let mut hits = Vec::new();
    for engine in ["https://ahmia.search", "http://xmhoy5xn3tcykynb.onion"] {
        if !hits.is_empty() {
            break;
        }
        match search_engine(engine, &refined, tor_socks.clone()).await {
            Ok(h) => hits = h,
            Err(e) => eprintln!("(engine {engine} failed: {e})"),
        }
    }

    // 3) Filter the most relevant hits with the LLM.
    let filtered = filter_hits(&provider, &refined, &hits).await.unwrap_or(hits.clone());

    // 4) Summarize findings with the LLM.
    let summary = summarize(&provider, &refined, &filtered)
        .await
        .unwrap_or_else(|_| "No LLM summary available (provider unreachable?).".into());

    Ok(Report { original_query: query.to_string(), refined_query: refined, hits: filtered, summary })
}

// ---- pipeline steps ------------------------------------------------------

async fn refine_query(p: &Provider, q: &str) -> Result<String> {
    let prompt = format!(
        "You are an OSINT analyst. Rewrite this dark web search query into 2-4 precise search terms that would surface relevant .onion marketplaces, forums, or leak sites. Reply with ONLY the refined query.\n\nQuery: {q}"
    );
    Ok(chat(p, &prompt).await?.trim().to_string())
}

async fn filter_hits(p: &Provider, q: &str, hits: &[Hit]) -> Result<Vec<Hit>> {
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let list: String = hits.iter().enumerate().map(|(i, h)| format!("{}. {} | {}\n", i + 1, h.title, h.snippet)).collect();
    let prompt = format!(
        "Given this search query and candidate results, return ONLY the numbers (comma-separated) of the most relevant results for OSINT. If none are relevant, reply 'none'.\n\nQuery: {q}\n\n{list}"
    );
    let answer = chat(p, &prompt).await?;
    if answer.trim().eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for tok in answer.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(n) = tok.parse::<usize>() {
            if n >= 1 && n <= hits.len() {
                out.push(hits[n - 1].clone());
            }
        }
    }
    // Fall back to the first few if parsing yielded nothing.
    if out.is_empty() {
        return Ok(hits.iter().take(5).cloned().collect());
    }
    Ok(out)
}

async fn summarize(p: &Provider, q: &str, hits: &[Hit]) -> Result<String> {
    let list: String = hits.iter().map(|h| format!("- {} — {}\n", h.title, h.snippet)).collect();
    let prompt = format!(
        "You are an OSINT analyst writing a concise threat-research summary. Based on the query and these dark web sources, produce a short, factual Markdown summary of what was found (or that nothing notable was found). Cite source titles inline. Keep it under 250 words.\n\nQuery: {q}\n\nSources:\n{list}"
    );
    Ok(chat(p, &prompt).await?)
}

// ---- LLM providers -------------------------------------------------------

/// Call the chosen provider and return its text response.
pub async fn chat(p: &Provider, prompt: &str) -> Result<String> {
    match p {
        Provider::OpenAI { model } => openai_chat(model, prompt).await,
        Provider::Claude { model } => claude_chat(model, prompt).await,
        Provider::Gemini { model } => gemini_chat(model, prompt).await,
        Provider::Ollama { model } => ollama_chat(model, prompt).await,
    }
}

async fn openai_chat(model: &str, prompt: &str) -> Result<String> {
    let key = std::env::var("OPENAI_API_KEY").context("set OPENAI_API_KEY")?;
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({ "model": model, "messages": [{ "role": "user", "content": prompt }] }))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    Ok(v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())
}

async fn claude_chat(model: &str, prompt: &str) -> Result<String> {
    let key = std::env::var("ANTHROPIC_API_KEY").context("set ANTHROPIC_API_KEY")?;
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({ "model": model, "max_tokens": 1024, "messages": [{ "role": "user", "content": prompt }] }))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    Ok(v["content"][0]["text"].as_str().unwrap_or("").to_string())
}

async fn gemini_chat(model: &str, prompt: &str) -> Result<String> {
    let key = std::env::var("GEMINI_API_KEY").context("set GEMINI_API_KEY")?;
    let client = reqwest::Client::new();
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={key}");
    let resp = client.post(&url).json(&json!({ "contents": [{ "parts": [{ "text": prompt }] }]})).send().await?;
    let v: serde_json::Value = resp.json().await?;
    Ok(v["candidates"][0]["content"]["parts"][0]["text"].as_str().unwrap_or("").to_string())
}

async fn ollama_chat(model: &str, prompt: &str) -> Result<String> {
    let base = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/chat"))
        .json(&json!({ "model": model, "stream": false, "messages": [{ "role": "user", "content": prompt }] }))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    Ok(v["message"]["content"].as_str().unwrap_or("").to_string())
}

// ---- dark web search over Tor -------------------------------------------

/// Search a .onion engine. Tries plain HTTP first, then headless browser (captcha/Cloudflare fallback).
async fn search_engine(engine: &str, query: &str, tor_socks: Option<String>) -> Result<Vec<Hit>> {
    let url = format!("{engine}/?q={}", urlencode(query));

    // Attempt 1: plain request through Tor.
    match try_fetch_html(&url, tor_socks.clone()).await {
        Ok(html) => {
            if let Ok(hits) = parse_hits(&html) {
                return Ok(hits);
            }
        }
        Err(e) => eprintln!("(plain fetch failed: {e})"),
    }

    // Attempt 2: headless browser (bypasses hCaptcha / reCAPTCHA / Cloudflare).
    eprintln!("(engine blocked — retrying with headless browser for captcha/Cloudflare…)");
    let page = fetch::fetch(&url, &FetchOptions::new().mode(FetchMode::Stealth)).await?;
    parse_hits(&page.html)
}

async fn try_fetch_html(url: &str, tor_socks: Option<String>) -> Result<String> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) Chrome/124.0.0.0 Safari/537.36");

    if let Some(socks) = tor_socks {
        // Route through Tor (SOCKS5). `all` applies it to http + https.
        builder = builder.proxy(reqwest::Proxy::all(&socks).expect("valid socks url"));
    }

    let client = builder.build()?;
    let resp = client.get(url).send().await?;
    Ok(resp.text().await?)
}

/// Parse search results from an engine's HTML (best-effort across engines).
fn parse_hits(html: &str) -> Result<Vec<Hit>> {
    let doc = scraper::Html::parse_fragment(html);
    let mut out = Vec::new();

    // Common result link patterns.
    for sel in ["a.result-title", "h3 a", ".result a", "a[href*='.onion']", "td a"] {
        if let Ok(s) = scraper::Selector::parse(sel) {
            for el in doc.select(&s).take(10) {
                if let Some(href) = el.value().attr("href") {
                    if href.contains(".onion") || !out.is_empty() {
                        let title: String = el.text().collect::<String>();
                        if !title.trim().is_empty() && out.len() < 10 {
                            out.push(Hit { title: clean(title), url: href.to_string(), snippet: String::new() });
                        }
                    }
                }
            }
        }
    }

    if out.is_empty() {
        return Err(anyhow!("no results parsed (engine layout may differ)"));
    }
    Ok(out)
}

fn clean(s: String) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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
