//! Parse RSS / Atom feeds into clean items (lightweight, no XML crate needed).

use anyhow::Result;
use regex::Regex;
use serde_json::json;
use crate::{web, youtube};
use web::http_client;

/// Heuristic: does this URL look like a feed?
pub fn looks_like_feed(url: &str) -> bool {
    let l = url.to_lowercase();
    l.contains("/rss") || l.contains("/feed") || l.ends_with(".xml") || l.contains("atom.xml") || l.contains("/feeds/")
}

/// Fetch and parse a feed.
pub async fn parse(url: &str, json_out: bool) -> Result<()> {
    let client = http_client()?;
    let body = client.get(url).send().await?.text().await?;
    let items = parse_feed(&body);

    if items.is_empty() {
        anyhow::bail!("no feed items found at {url} (is it a valid RSS/Atom URL?)");
    }

    let mut lines = Vec::new();
    for it in &items {
        lines.push(format!(
            "{}\n   {}\n   {}",
            it["title"].as_str().unwrap_or(""),
            it.get("date").and_then(|d| d.as_str()).unwrap_or(""),
            it["link"].as_str().unwrap_or("")
        ));
    }

    web::emit(
        json_out,
        &json!({ "feed": url, "count": items.len(), "items": items }),
        &lines.join("\n\n"),
    )
}

/// Extract feed items from RSS or Atom XML.
fn parse_feed(xml: &str) -> Vec<serde_json::Value> {
    // <item>/<entry> blocks never nest in each other, so a simple open→close scan works.
    let mut items = Vec::new();
    for tag in ["item", "entry"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut start = 0usize;
        while let Some(rel) = xml[start..].find(&open) {
            let abs = start + rel;
            // Must be a real tag boundary (next char is space, '>', or '/').
            let after = &xml[abs + open.len()..];
            if !after.is_empty() && !matches!(after.as_bytes()[0], b' ' | b'>' | b'/') {
                start = abs + 1;
                continue;
            }
            match xml[abs..].find(&close) {
                Some(close_rel) => {
                    let inner_end = abs + close_rel;
                    let inner_start = abs + open.len();
                    // Drop the opening tag's trailing '>' (and any attributes).
                    let gt = xml[inner_start..inner_end].find('>').map(|g| inner_start + g + 1).unwrap_or(inner_start);
                    let inner = &xml[gt..inner_end];

                    let title = tag_text(inner, "title").unwrap_or_default();
                    let link = feed_link(inner);
                    let desc = tag_text(inner, "description")
                        .or_else(|| tag_text(inner, "summary"))
                        .map(|d| strip_html(&d))
                        .unwrap_or_default();
                    let date = tag_text(inner, "pubDate")
                        .or_else(|| tag_text(inner, "published"))
                        .or_else(|| tag_text(inner, "updated"))
                        .unwrap_or_default();

                    items.push(json!({
                        "title": title.trim(),
                        "link": link,
                        "description": desc.trim(),
                        "date": date.trim(),
                    }));

                    start = inner_end + close.len();
                }
                None => break, // no matching close tag; stop scanning this type
            }
        }
    }
    items
}

/// Extract the feed's primary link: <link>text</link>, Atom <link href=".."/>, or <guid>.
fn feed_link(inner: &str) -> String {
    // RSS: <link>http://...</link>
    if let Some(re) = Regex::new(r"<link>\s*(https?://[^<\s]+)\s*</link>").ok() {
        if let Some(m) = re.captures(inner) {
            return m[1].to_string();
        }
    }
    // Atom: <link href="http://..." rel="alternate"/> or without rel.
    if let Ok(re) = Regex::new(r#"<link[^>]*href="(https?://[^"]+)""#) {
        for m in re.captures_iter(inner) {
            return m[1].to_string();
        }
    }
    // guid fallback.
    tag_text(inner, "guid").unwrap_or_default()
}

/// Get the text content of a simple `<tag>...</tag>` (first occurrence).
fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let re = Regex::new(&format!(r#"<{tag}\b[^>]*>(.*?)</{tag}>"#)).ok()?;
    re.captures(xml).map(|c| c[1].trim().to_string())
}

/// Strip HTML tags + decode a few common entities.
fn strip_html(s: &str) -> String {
    let no_tags = s.replace('<', " ").replace('>', " ");
    let decoded = no_tags
        .replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss_items() {
        let xml = r#"<rss><channel>
            <item><title>Hello</title><link>http://a/1</link><description>First post</description><pubDate>Mon, 01 Jan 2024</pubDate></item>
            <item><title>World</title><link>http://a/2</link><description>&lt;b&gt;Bold&lt;/b&gt;</description></item>
        </channel></rss>"#;
        let items = parse_feed(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], "Hello");
        assert_eq!(items[1]["link"], "http://a/2");
        assert_eq!(items[1]["description"], "<b>Bold</b>");
    }

    #[test]
    fn parses_atom_entries() {
        let xml = r#"<feed><entry>
            <title>Note</title>
            <link href="http://x/9"/>
            <summary>Atom body</summary>
            <published>2024-05-01T00:00:00Z</published>
        </entry></feed>"#;
        let items = parse_feed(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["link"], "http://x/9");
        assert_eq!(items[0]["date"], "2024-05-01T00:00:00Z");
    }

    #[test]
    fn detects_feed_urls() {
        assert!(looks_like_feed("https://example.com/feed"));
        assert!(!looks_like_feed("https://example.com/page"));
    }
}
