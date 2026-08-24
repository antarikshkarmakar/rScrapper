//! YouTube without an API key: subtitles + video search via public pages.

use crate::web;
use anyhow::{anyhow, Result};
use serde_json::json;
use web::http_client;

/// Extract a YouTube video ID from a URL or bare ID.
pub fn extract_video_id(input: &str) -> Option<String> {
    let t = input.trim();
    // Bare 11-char ID.
    if t.len() == 11 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Some(t.to_string());
    }
    for pat in ["v=", "youtu.be/", "/embed/", "/shorts/"] {
        if let Some(pos) = t.find(pat) {
            let rest = &t[pos + pat.len()..];
            let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
            if id.len() == 11 {
                return Some(id);
            }
        }
    }
    None
}

/// Fetch subtitles/transcript for a video. Tries the watch page's caption track,
/// then falls back to the timedtext endpoint with common languages.
pub async fn subs(video: &str, json_out: bool) -> Result<()> {
    let id = extract_video_id(video).ok_or_else(|| anyhow!("could not find a YouTube video ID in `{video}`"))?;

    // 1) Load watch page and look for captionTracks URL.
    let client = http_client()?;
    let watch = format!("https://www.youtube.com/watch?v={id}");
    let html = client.get(&watch).send().await?.text().await?;

    if let Some(track_url) = find_caption_track(&html) {
        let transcript = fetch_transcript(&client, &track_url).await.unwrap_or_default();
        if !transcript.is_empty() {
            return web::emit(json_out, &json!({ "video": id, "url": watch, "subtitles": transcript }), &transcript);
        }
    }

    // 2) Fallback: timedtext endpoint with a few languages.
    for lang in ["en", "en-US"] {
        let tt = format!("https://www.youtube.com/api/timedtext?v={id}&lang={lang}");
        if let Ok(t) = fetch_transcript(&client, &tt).await {
            if !t.is_empty() {
                return web::emit(json_out, &json!({ "video": id, "url": watch, "subtitles": t }), &t);
            }
        }
    }

    Err(anyhow!(
        "no subtitles found for `{id}` (the video may have no captions). Try `rscraper youtube search` to confirm the ID."
    ))
}

/// Find a captionTracks base URL inside the watch page's ytInitialPlayerResponse.
fn find_caption_track(html: &str) -> Option<String> {
    let key = "\"captionTracks\":";
    let start = html.find(key)? + key.len();
    // Grab up to the first "baseurl" value.
    let rest = &html[start..];
    let bu = rest.find("\"baseUrl\"")?;
    let after = &rest[bu..];
    let quote_start = after.find('"')? + 1;
    let tail = &after[quote_start..];
    let end = tail.find('"')?;
    Some(tail[..end].replace("\\/", "/"))
}

/// Download a caption track and strip XML tags into plain lines.
async fn fetch_transcript(client: &reqwest::Client, url: &str) -> Result<String> {
    let body = client.get(url).send().await?.text().await?;
    // Captions are <text start=.. dur=..>word</text>; strip tags + entities.
    let plain: String = body
        .split('<')
        .filter_map(|seg| seg.strip_prefix("text "))
        .map(|s| s.split('>').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");

    if plain.trim().is_empty() {
        // Maybe it was already plain text.
        let stripped: String = body.chars().filter(|c| !(*c == '<' || *c == '>')).collect();
        Ok(stripped)
    } else {
        Ok(plain.replace("&amp;", "&").replace("&#39;", "'").replace("&quot;", "\""))
    }
}

/// Search YouTube videos by scraping the results page.
pub async fn search(query: &str, n: usize, json_out: bool) -> Result<()> {
    let client = http_client()?;
    let url = format!("https://www.youtube.com/results?search_query={}", web::urlencode(query));
    let html = client.get(&url).send().await?.text().await?;

    // Video titles live in <a ... href="/watch?v=ID">TITLE</a> inside ytd-video-renderer.
    let mut seen = std::collections::HashSet::new();
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut text_lines: Vec<String> = Vec::new();

    // Simple scan for /watch?v= links followed by a title string in ytInitialData.
    let data_start = html.find("ytInitialData").map(|p| p + "ytInitialData".len()).unwrap_or(0);
    let chunk = &html[data_start..];
    let mut idx = 0usize;
    while let Some(pos) = chunk[idx..].find("\"videoRenderer\"") {
        let seg_start = idx + pos;
        // Find the next "title":{"simpleText":"..." within a window.
        let window = &chunk[seg_start..(seg_start + 4000).min(chunk.len())];
        if let Some(t) = extract_simple_text(window, "\"title\"") {
            // Find videoId in this segment.
            if let Some(id) = find_in_window(window, "\"videoId\":\"", 11) {
                if seen.insert(id.clone()) && results.len() < n {
                    let link = format!("https://www.youtube.com/watch?v={id}");
                    results.push(json!({ "title": t, "url": link }));
                    text_lines.push(format!("{} — {}", t, link));
                }
            }
        }
        idx = seg_start + 1;
    }

    if results.is_empty() {
        return Err(anyhow!("no YouTube results parsed (layout may have changed). Try a different query."));
    }
    web::emit(json_out, &json!({ "query": query, "count": results.len(), "results": results }), &text_lines.join("\n"))
}

/// Extract `"key":{"simpleText":"VALUE"}` from a JSON-ish window.
fn extract_simple_text(window: &str, key: &str) -> Option<String> {
    let pos = window.find(key)?;
    let rest = &window[pos + key.len()..];
    let st = rest.find("\"simpleText\":\"")? + "\"simpleText\":\"".len();
    let tail = &rest[st..];
    let end = tail.find('"')?;
    Some(tail[..end].replace("\\u0026", "&").replace("\\/", "/"))
}

/// Find a fixed-length token after `needle` in a window.
fn find_in_window(window: &str, needle: &str, len: usize) -> Option<String> {
    let pos = window.find(needle)? + needle.len();
    let tail = &window[pos..];
    let id: String = tail.chars().take(len).collect();
    if id.len() == len && id.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(id)
    } else {
        None
    }
}
