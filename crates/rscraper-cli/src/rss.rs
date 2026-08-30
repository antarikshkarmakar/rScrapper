//! Parse RSS, Atom, and JSON Feed bytes into clean bounded items.

use crate::{context::AppContext, web};
use anyhow::Result;
use feed_rs::model::{Content, Entry, Link, Text};
use quick_xml::encoding::Decoder;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::XmlVersion;
use rscraper_core::markdown::{html_to_markdown_with_options, MarkdownOptions};
use rscraper_core::{truncate_chars, Error, FetchRequest, Result as CoreResult};
use serde::Serialize;
use serde_json::json;
use url::Url;

const DEFAULT_FEED_ITEMS: usize = 20;
const MAX_FEED_ITEMS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 8_192;

#[derive(Debug, Clone, Serialize)]
pub struct FeedItem {
    pub title: String,
    pub link: String,
    pub description: String,
    pub date: String,
}

/// Heuristic: does this URL look like a feed?
pub fn looks_like_feed(url: &str) -> bool {
    let l = url.to_lowercase();
    l.contains("/rss")
        || l.contains("/feed")
        || l.ends_with(".xml")
        || l.contains("atom.xml")
        || l.contains("/feeds/")
}

/// Fetch and parse a feed.
pub async fn parse(url: &str, json_out: bool) -> Result<()> {
    let context = AppContext::try_default()?;
    parse_with_context(&context, url, json_out).await
}

pub async fn parse_with_context(context: &AppContext, url: &str, json_out: bool) -> Result<()> {
    let items = fetch_feed_items_with_context(context, url, DEFAULT_FEED_ITEMS).await?;

    if items.is_empty() {
        anyhow::bail!("no feed items found at {url} (is it a valid RSS/Atom URL?)");
    }

    let mut lines = Vec::new();
    for it in &items {
        lines.push(format!("{}\n   {}\n   {}", it.title, it.date, it.link));
    }

    web::emit(
        json_out,
        &json!({ "feed": url, "count": items.len(), "items": items }),
        &lines.join("\n\n"),
    )
}

pub async fn fetch_feed_items_with_context(
    context: &AppContext,
    url: &str,
    limit: usize,
) -> CoreResult<Vec<FeedItem>> {
    let response = context
        .fetch
        .fetch_raw_request(FetchRequest::request(url)?)
        .await?;
    parse_feed_bytes(&response.bytes, &response.url, limit)
}

pub fn parse_feed_bytes(bytes: &[u8], feed_url: &Url, limit: usize) -> CoreResult<Vec<FeedItem>> {
    reject_xml_dtd(bytes)?;
    let explicit_fallback_links = explicit_xml_fallback_links(bytes, feed_url)?;
    let feed = feed_rs::parser::Builder::new()
        .base_uri(Some(feed_url.as_str()))
        .build()
        .parse(bytes)
        .map_err(|error| Error::Parse {
            kind: "feed",
            message: error.to_string(),
        })?;
    let limit = limit.min(MAX_FEED_ITEMS);
    let mut items = Vec::new();

    for (index, entry) in feed.entries.into_iter().enumerate() {
        if items.len() >= limit {
            break;
        }
        if let Some(item) = normalize_entry(
            entry,
            feed_url,
            explicit_fallback_links
                .get(index)
                .and_then(|fallback| fallback.as_deref()),
        ) {
            items.push(item);
        }
    }

    Ok(items)
}

fn reject_xml_dtd(bytes: &[u8]) -> CoreResult<()> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_none_or(|byte| *byte != b'<')
    {
        return Ok(());
    }

    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::DocType(_)) => {
                return Err(Error::Parse {
                    kind: "feed",
                    message: "feed DTDs are not supported".into(),
                });
            }
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(error) => {
                return Err(Error::Parse {
                    kind: "feed",
                    message: error.to_string(),
                });
            }
        }
        buffer.clear();
    }
}

fn explicit_xml_fallback_links(bytes: &[u8], feed_url: &Url) -> CoreResult<Vec<Option<String>>> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_none_or(|byte| *byte != b'<')
    {
        return Ok(Vec::new());
    }

    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut current: Option<XmlEntryFallback> = None;
    let mut links = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) => {
                depth += 1;
                let local_name = start.local_name();
                let name = local_name.as_ref();
                if current.is_none() && matches!(name, b"item" | b"entry") {
                    current = Some(XmlEntryFallback::new(
                        if name == b"item" {
                            XmlEntryKind::Rss
                        } else {
                            XmlEntryKind::Atom
                        },
                        depth,
                    ));
                } else if let Some(entry) = &mut current {
                    if depth == entry.depth + 1 {
                        match (entry.kind, name) {
                            (XmlEntryKind::Rss, b"guid") => {
                                entry.capture = Some(XmlCapture {
                                    text: String::new(),
                                    usable_as_link: rss_guid_is_permalink(
                                        &start,
                                        reader.decoder(),
                                    )?,
                                });
                            }
                            (XmlEntryKind::Atom, b"id") => {
                                entry.capture = Some(XmlCapture {
                                    text: String::new(),
                                    usable_as_link: true,
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(capture) = current.as_mut().and_then(|entry| entry.capture.as_mut()) {
                    capture
                        .text
                        .push_str(&text.decode().map_err(xml_parse_error)?);
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(capture) = current.as_mut().and_then(|entry| entry.capture.as_mut()) {
                    capture
                        .text
                        .push_str(&text.decode().map_err(xml_parse_error)?);
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(capture) = current.as_mut().and_then(|entry| entry.capture.as_mut()) {
                    capture.text.push_str(&decode_xml_reference(&reference)?);
                }
            }
            Ok(Event::End(end)) => {
                let local_name = end.local_name();
                let name = local_name.as_ref();
                if let Some(entry) = &mut current {
                    if depth == entry.depth + 1
                        && matches!(
                            (entry.kind, name),
                            (XmlEntryKind::Rss, b"guid") | (XmlEntryKind::Atom, b"id")
                        )
                    {
                        if let Some(capture) = entry.capture.take() {
                            if capture.usable_as_link {
                                let normalized =
                                    normalize_whitespace(&decode_entities(&capture.text));
                                if resolve_link(&normalized, feed_url).is_some() {
                                    entry.fallback = Some(normalized);
                                }
                            }
                        }
                    }
                    if depth == entry.depth
                        && matches!(
                            (entry.kind, name),
                            (XmlEntryKind::Rss, b"item") | (XmlEntryKind::Atom, b"entry")
                        )
                    {
                        links.push(entry.fallback.clone());
                        current = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Empty(empty)) => {
                depth += 1;
                let local_name = empty.local_name();
                let name = local_name.as_ref();
                if current.is_none() && matches!(name, b"item" | b"entry") {
                    links.push(None);
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::DocType(_)) => {
                return Err(Error::Parse {
                    kind: "feed",
                    message: "feed DTDs are not supported".into(),
                });
            }
            Ok(Event::Eof) => return Ok(links),
            Ok(_) => {}
            Err(error) => return Err(xml_parse_error(error)),
        }
        buffer.clear();
    }
}

#[derive(Clone, Copy)]
enum XmlEntryKind {
    Rss,
    Atom,
}

struct XmlEntryFallback {
    kind: XmlEntryKind,
    depth: usize,
    fallback: Option<String>,
    capture: Option<XmlCapture>,
}

impl XmlEntryFallback {
    fn new(kind: XmlEntryKind, depth: usize) -> Self {
        Self {
            kind,
            depth,
            fallback: None,
            capture: None,
        }
    }
}

struct XmlCapture {
    text: String,
    usable_as_link: bool,
}

fn rss_guid_is_permalink(
    start: &quick_xml::events::BytesStart<'_>,
    decoder: Decoder,
) -> CoreResult<bool> {
    for attribute in start.attributes() {
        let attribute = attribute.map_err(xml_parse_error)?;
        if attribute.key.as_ref() != b"isPermaLink" {
            continue;
        }
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, decoder)
            .map_err(xml_parse_error)?;
        return Ok(!value.eq_ignore_ascii_case("false"));
    }
    Ok(true)
}

fn normalize_entry(
    entry: Entry,
    feed_url: &Url,
    explicit_fallback_link: Option<&str>,
) -> Option<FeedItem> {
    let title = entry
        .title
        .as_ref()
        .map(|text| normalize_whitespace(&decode_entities(&text.content)))
        .unwrap_or_default();
    let link = primary_link(&entry.links)
        .and_then(|link| resolve_link(&link.href, feed_url))
        .or_else(|| explicit_fallback_link.and_then(|link| resolve_link(link, feed_url)))
        .unwrap_or_default();
    let description = entry
        .summary
        .as_ref()
        .and_then(|text| render_text(text, feed_url))
        .or_else(|| {
            entry
                .content
                .as_ref()
                .and_then(|content| render_content(content, feed_url))
        })
        .unwrap_or_default();
    let date = entry
        .published
        .as_ref()
        .or(entry.updated.as_ref())
        .map(|date| date.to_rfc3339())
        .unwrap_or_default();

    if title.is_empty() && link.is_empty() && description.is_empty() {
        return None;
    }

    Some(FeedItem {
        title,
        link,
        description,
        date,
    })
}

fn primary_link(links: &[Link]) -> Option<&Link> {
    links
        .iter()
        .find(|link| link.rel.as_deref() == Some("alternate"))
        .or_else(|| links.iter().find(|link| link.rel.is_none()))
        .or_else(|| links.first())
}

fn resolve_link(candidate: &str, feed_url: &Url) -> Option<String> {
    if candidate.trim().is_empty() {
        return None;
    }
    feed_url
        .join(candidate.trim())
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| url.to_string())
}

fn render_text(text: &Text, feed_url: &Url) -> Option<String> {
    if is_html_type(text.content_type.as_str()) {
        render_html(&text.content, feed_url)
    } else {
        Some(normalize_whitespace(&decode_entities(&text.content)))
    }
}

fn render_content(content: &Content, feed_url: &Url) -> Option<String> {
    let body = content.body.as_ref()?;
    if is_html_type(content.content_type.as_str()) {
        render_html(body, feed_url)
    } else {
        Some(normalize_whitespace(&decode_entities(body)))
    }
}

fn render_html(html: &str, feed_url: &Url) -> Option<String> {
    html_to_markdown_with_options(
        html,
        &MarkdownOptions {
            base_url: Some(feed_url.clone()),
            max_chars: MAX_DESCRIPTION_CHARS,
        },
    )
    .ok()
    .map(|markdown| markdown.trim().replace("\\&", "&"))
    .filter(|description| !description.is_empty())
}

fn is_html_type(content_type: &str) -> bool {
    content_type.eq_ignore_ascii_case("text/html")
        || content_type.eq_ignore_ascii_case("application/xhtml+xml")
}

fn decode_entities(input: &str) -> String {
    quick_xml::escape::unescape(input)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| input.to_string())
}

fn decode_xml_reference(reference: &quick_xml::events::BytesRef<'_>) -> CoreResult<String> {
    if let Some(ch) = reference.resolve_char_ref().map_err(xml_parse_error)? {
        return Ok(ch.to_string());
    }
    let name = reference.decode().map_err(xml_parse_error)?;
    Ok(resolve_predefined_entity(&name)
        .map(str::to_string)
        .unwrap_or_else(|| {
            let mut escaped = String::from("&");
            escaped.push_str(&name);
            escaped.push(';');
            escaped
        }))
}

fn xml_parse_error(error: impl std::fmt::Display) -> Error {
    Error::Parse {
        kind: "feed",
        message: error.to_string(),
    }
}

fn normalize_whitespace(input: &str) -> String {
    truncate_chars(
        &input.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_DESCRIPTION_CHARS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss_items() {
        let xml = r#"<rss version="2.0"><channel>
            <item><title>Hello</title><link>http://a/1</link><description>First post</description><pubDate>Mon, 01 Jan 2024</pubDate></item>
            <item><title>World</title><link>http://a/2</link><description>&lt;b&gt;Bold&lt;/b&gt;</description></item>
        </channel></rss>"#;
        let items = parse_feed_bytes(
            xml.as_bytes(),
            &Url::parse("http://a/feed.xml").unwrap(),
            20,
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Hello");
        assert_eq!(items[1].link, "http://a/2");
        assert_eq!(items[1].description, "**Bold**");
    }

    #[test]
    fn parses_atom_entries() {
        let xml = r#"<feed><entry>
            <title>Note</title>
            <link href="http://x/9"/>
            <summary>Atom body</summary>
            <published>2024-05-01T00:00:00Z</published>
        </entry></feed>"#;
        let items = parse_feed_bytes(
            xml.as_bytes(),
            &Url::parse("http://x/feed.xml").unwrap(),
            20,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].link, "http://x/9");
        assert_eq!(items[0].date, "2024-05-01T00:00:00+00:00");
    }

    #[test]
    fn detects_feed_urls() {
        assert!(looks_like_feed("https://example.com/feed"));
        assert!(!looks_like_feed("https://example.com/page"));
    }
}
