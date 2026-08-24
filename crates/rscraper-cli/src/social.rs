//! Optional social platforms. Public ones work without login; cookie-based ones
//! store cookies **locally** in the config dir and guide you through setup.

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use std::fs;
use crate::{web, youtube};
use web::http_client;

/// Local state directory (cookies live here — never uploaded anywhere).
fn home() -> std::path::PathBuf {
    std::env::var("RSCRAPER_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default().join(".rscraper"))
}

fn cookie_path(platform: &str) -> std::path::PathBuf {
    home().join(format!("{platform}.cookies.txt"))
}

/// Load a platform's cookies into reqwest `Cookie` header format.
fn load_cookies(platform: &str) -> Result<String> {
    let p = cookie_path(platform);
    if !p.exists() {
        return Err(anyhow!(
            "no cookies for `{platform}` yet — run `rscraper setup {platform}` to add them step by step.".to_string(),
        ));
    }
    // Accept either a raw Cookie header value or an N-line "name=value" list.
    let raw = fs::read_to_string(&p).context("reading cookie file")?;
    if raw.lines().count() <= 1 {
        Ok(raw.trim().to_string())
    } else {
        // Convert name=value lines into a single Cookie header.
        Ok(raw
            .lines()
            .map(|l| l.splitn(2, '=').collect::<Vec<_>>().join("="))
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; "))
    }
}

/// Guided setup: explain exactly what a platform needs and where to put it.
pub fn setup(platform: &str, json_out: bool) -> Result<()> {
    let p = platform.to_lowercase();
    let (needs_login, steps): (&str, Vec<String>) = match p.as_str() {
        "twitter" | "x" => ("yes", vec![
            "1. Log in to https://twitter.com in your normal browser.".to_string(),
            "2. Open DevTools → Application → Cookies → twitter.com.".to_string(),
            "3. Copy the value of `auth_token` (and ideally `ct0`).".to_string(),
            "4. Paste them into a file, one per line as name=value:".to_string(),
            format!("     auth_token=<paste>\n     ct0=<paste>"),
            format!("5. Save to: {}", cookie_path("twitter").display()),
        ]),
        "reddit" => ("optional", vec![
            "Reddit works without login for public posts/search.".to_string(),
            "If you hit rate limits, add a `reddit_session` cookie:".to_string(),
            "1. Log in at https://www.reddit.com → DevTools → Cookies.".to_string(),
            format!("2. Save `reddit_session=<value>` to: {}", cookie_path("reddit").display()),
        ]),
        "bilibili" => ("no", vec![
            "Bilibili search uses a public API — no login needed.".to_string(),
            "Just run: rscraper bilibili <query>".to_string(),
        ]),
        "xiaohongshu" | "xhs" => ("yes", vec![
            "1. Log in at https://www.xiaohongshu.com in your browser.".to_string(),
            "2. DevTools → Cookies → xiaohongshu.com → copy `web_session`.".to_string(),
            format!("3. Save `web_session=<value>` to: {}", cookie_path("xiaohongshu").display()),
        ]),
        "linkedin" => ("yes", vec![
            "1. Log in at https://www.linkedin.com in your browser.".to_string(),
            "2. DevTools → Cookies → linkedin.com → copy `li_at` (and `JSESSIONID`).".to_string(),
            format!("3. Save them to: {}", cookie_path("linkedin").display()),
        ]),
        other => return Err(anyhow!(
            "unknown platform `{other}`. Supported: twitter, reddit, bilibili, xiaohongshu, linkedin".to_string(),
        )),
    };

    let text = format!(
        "Setup for {p} (login required: {needs_login})\n\n{}\n\nCookies stay local in {} — they are only sent to {p}.",
        steps.join("\n"),
        home().display()
    );
    web::emit(json_out, &json!({ "platform": p, "needs_login": needs_login, "steps": steps }), &text)
}

// ---- platform readers ----------------------------------------------------

/// Twitter/X search or timeline (cookies required).
pub async fn twitter(query: Option<&str>, json_out: bool) -> Result<()> {
    let cookie = load_cookies("twitter")?;
    let client = http_client()?;
    let q = query.unwrap_or("");
    // Use the syndication endpoint which is more bot-tolerant.
    let url = if q.is_empty() {
        "https://syndication.twitter.com/srv/timeline-profile/screen-name/elonmusk".to_string()
    } else {
        format!("https://twitter.com/search?q={}", web::urlencode(q))
    };

    let resp = client.get(&url).header("Cookie", &cookie).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "Twitter returned HTTP {} — cookies may be expired. Re-run `rscraper setup twitter`.",
            resp.status()
        ));
    }
    let body = resp.text().await?;
    let md = rscraper_core::html_to_markdown(&body);
    web::emit(json_out, &json!({ "platform": "twitter", "query": q, "content": md }), &md)
}

/// Reddit public search (works without login).
pub async fn reddit(query: &str, n: usize, json_out: bool) -> Result<()> {
    let client = http_client()?;
    // JSON API is the most reliable route.
    let url = format!("https://www.reddit.com/search.json?q={}&limit={}", web::urlencode(query), n);
    let mut req = client.get(&url).header("User-Agent", "rscraper/0.1 (local research tool)");

    // Optional cookie if present.
    if let Ok(c) = load_cookies("reddit") {
        req = req.header("Cookie", c);
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("Reddit returned HTTP {} (rate-limited?). Try again later or add a cookie via `rscraper setup reddit`.", resp.status()));
    }
    let v: serde_json::Value = resp.json().await?;

    let mut results = Vec::new();
    let mut lines = Vec::new();
    if let Some(children) = v["data"]["children"].as_array() {
        for c in children.iter().take(n) {
            let d = &c["data"];
            let title = d["title"].as_str().unwrap_or("").to_string();
            let link = format!("https://www.reddit.com{}", d["permalink"].as_str().unwrap_or(""));
            let score = d["score"].as_i64().unwrap_or(0);
            results.push(json!({ "title": title, "url": link, "score": score }));
            lines.push(format!("{} (▲{})\n   {}", title, score, link));
        }
    }

    if results.is_empty() {
        return web::emit(json_out, &json!({ "query": query, "count": 0 }), "No Reddit results.");
    }
    web::emit(json_out, &json!({ "query": query, "count": results.len(), "results": results }), &lines.join("\n"))
}

/// Bilibili video search (public API).
pub async fn bilibili(query: &str, n: usize, json_out: bool) -> Result<()> {
    let client = http_client()?;
    let url = format!(
        "https://api.bilibili.com/x/web-interface/search/type?search_type=video&keyword={}",
        web::urlencode(query)
    );
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("Bilibili returned HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await?;

    let mut results = Vec::new();
    let mut lines = Vec::new();
    if let Some(list) = v["data"]["result"].as_array() {
        for item in list.iter().take(n) {
            let title = item["title"]
                .as_str()
                .unwrap_or("")
                .replace("<em class=\"keyword\">", "")
                .replace("</em>", "");
            let bvid = item["bvid"].as_str().unwrap_or("");
            let link = format!("https://www.bilibili.com/video/{bvid}");
            results.push(json!({ "title": title, "url": link }));
            lines.push(format!("{} — {}", title, link));
        }
    }

    if results.is_empty() {
        return web::emit(json_out, &json!({ "query": query, "count": 0 }), "No Bilibili results.");
    }
    web::emit(json_out, &json!({ "query": query, "count": results.len(), "results": results }), &lines.join("\n"))
}

/// Xiaohongshu (cookies required).
pub async fn xiaohongshu(query: &str, json_out: bool) -> Result<()> {
    let cookie = load_cookies("xiaohongshu")?;
    let client = http_client()?;
    let url = format!("https://www.xiaohongshu.com/search_result?keyword={}", web::urlencode(query));
    let resp = client.get(&url).header("Cookie", &cookie).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "Xiaohongshu returned HTTP {} — cookies may be expired. Re-run `rscraper setup xiaohongshu`.",
            resp.status()
        ));
    }
    let body = resp.text().await?;
    let md = rscraper_core::html_to_markdown(&body);
    web::emit(json_out, &json!({ "platform": "xiaohongshu", "query": query, "content": md }), &md)
}

/// LinkedIn (cookies required).
pub async fn linkedin(query: &str, json_out: bool) -> Result<()> {
    let cookie = load_cookies("linkedin")?;
    let client = http_client()?;
    let url = format!("https://www.linkedin.com/search/results/people/?keywords={}", web::urlencode(query));
    let resp = client.get(&url).header("Cookie", &cookie).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "LinkedIn returned HTTP {} — cookies may be expired. Re-run `rscraper setup linkedin`.",
            resp.status()
        ));
    }
    let body = resp.text().await?;
    let md = rscraper_core::html_to_markdown(&body);
    web::emit(json_out, &json!({ "platform": "linkedin", "query": query, "content": md }), &md)
}
