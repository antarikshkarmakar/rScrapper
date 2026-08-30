//! YouTube without an API key: subtitles + video search via public pages.

use crate::{context::AppContext, web};
use anyhow::{anyhow, Result};
use quick_xml::encoding::Decoder;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::XmlVersion;
use rscraper_core::{
    truncate_chars, Error, FetchHostRestriction, FetchRequest, Result as CoreResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use url::Url;

const MAX_CAPTION_TRACKS: usize = 100;
const MAX_CAPTION_EVENTS: usize = 10_000;
const MAX_CAPTION_CHARS: usize = 100_000;
const MAX_JSON_NODES: usize = 20_000;
const MAX_SEARCH_RESULTS: usize = 100;
const CAPTION_GAP_MS: i64 = 1_500;
const VIDEO_ID_LEN: usize = 11;
const MAX_NESTED_URL_DECODE_DEPTH: usize = 4;
const MAX_NESTED_URL_VALUE_BYTES: usize = 4096;
const CAPTION_ALLOWED_HOST_SUFFIXES: &[&str] = &[
    "youtube.com",
    "youtube-nocookie.com",
    "google.com",
    "googlevideo.com",
];

#[derive(Debug, Clone)]
pub struct CaptionTrack {
    pub base_url: Url,
    pub language_code: String,
    pub name: String,
    pub is_generated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct YoutubeSearchResult {
    pub title: String,
    pub url: String,
}

/// Extract a YouTube video ID from a URL or bare ID.
pub fn extract_video_id(input: &str) -> Option<String> {
    let t = input.trim();
    if is_valid_video_id(t) {
        return Some(t.to_string());
    }
    for pat in ["v=", "youtu.be/", "/embed/", "/shorts/"] {
        if let Some(pos) = t.find(pat) {
            let rest = &t[pos + pat.len()..];
            let id: String = rest.chars().take_while(|c| is_video_id_char(*c)).collect();
            if is_valid_video_id(&id) {
                return Some(id);
            }
        }
    }
    None
}

/// Fetch subtitles/transcript for a video. Tries the watch page's caption track,
/// then falls back to the timedtext endpoint with common languages.
pub async fn subs(video: &str, json_out: bool) -> Result<()> {
    let context = AppContext::try_default()?;
    subs_with_context(&context, video, json_out).await
}

pub async fn subs_with_context(context: &AppContext, video: &str, json_out: bool) -> Result<()> {
    let id = extract_video_id(video)
        .ok_or_else(|| anyhow!("could not find a YouTube video ID in `{video}`"))?;

    let watch = format!("https://www.youtube.com/watch?v={id}");
    let page = context
        .fetch
        .fetch_request(FetchRequest::request(&watch)?)
        .await?;

    if let Ok(tracks) = parse_caption_tracks(&page.html) {
        if let Some(track) = select_caption_track(&tracks, None) {
            let transcript = fetch_transcript(context, track).await.unwrap_or_default();
            if !transcript.is_empty() {
                return web::emit(
                    json_out,
                    &json!({ "video": id, "url": watch, "subtitles": transcript }),
                    &transcript,
                );
            }
        }
    }

    for lang in ["en", "en-US"] {
        let tt = format!("https://www.youtube.com/api/timedtext?v={id}&lang={lang}");
        let track = CaptionTrack {
            base_url: validate_caption_url(&tt)?,
            language_code: lang.to_string(),
            name: lang.to_string(),
            is_generated: false,
        };
        if let Ok(t) = fetch_transcript(context, &track).await {
            if !t.is_empty() {
                return web::emit(
                    json_out,
                    &json!({ "video": id, "url": watch, "subtitles": t }),
                    &t,
                );
            }
        }
    }

    Err(anyhow!(
        "no subtitles found for `{id}` (the video may have no captions). Try `rscraper youtube search` to confirm the ID."
    ))
}

async fn fetch_transcript(context: &AppContext, track: &CaptionTrack) -> CoreResult<String> {
    let json3_url = with_json3_format(track.base_url.clone());
    let mut request = FetchRequest::request(json3_url.as_str())?;
    request.host_restriction = Some(caption_host_restriction()?);
    let response = context.fetch.fetch_raw_request(request).await?;
    parse_json3_captions(&response.bytes).or_else(|_| parse_xml_captions(&response.bytes))
}

/// Search YouTube videos by parsing the embedded page data.
pub async fn search(query: &str, n: usize, json_out: bool) -> Result<()> {
    let context = AppContext::try_default()?;
    search_with_context(&context, query, n, json_out).await
}

pub async fn search_with_context(
    context: &AppContext,
    query: &str,
    n: usize,
    json_out: bool,
) -> Result<()> {
    let mut url = Url::parse("https://www.youtube.com/results")?;
    url.query_pairs_mut().append_pair("search_query", query);
    let page = context
        .fetch
        .fetch_request(FetchRequest::request(url.as_str())?)
        .await?;
    let results = parse_search_results(&page.html, n)?;

    if results.is_empty() {
        return Err(anyhow!(
            "no YouTube results parsed (layout may have changed). Try a different query."
        ));
    }

    let text_lines: Vec<String> = results
        .iter()
        .map(|result| format!("{} — {}", result.title, result.url))
        .collect();
    web::emit(
        json_out,
        &json!({ "query": query, "count": results.len(), "results": results }),
        &text_lines.join("\n"),
    )
}

pub fn parse_caption_tracks(html: &str) -> CoreResult<Vec<CaptionTrack>> {
    let value = extract_named_json(
        html,
        &["ytInitialPlayerResponse", "playerResponse"],
        value_has_caption_tracks,
    )?;
    let response: PlayerResponse = serde_json::from_value(value).map_err(json_parse_error)?;
    let raw_tracks = response
        .captions
        .and_then(|captions| captions.player_captions_tracklist_renderer)
        .map(|renderer| renderer.caption_tracks)
        .unwrap_or_default();

    let mut tracks = Vec::new();
    for raw in raw_tracks.into_iter().take(MAX_CAPTION_TRACKS) {
        let Some(base_url) = raw.base_url else {
            continue;
        };
        let language_code = raw.language_code.unwrap_or_default();
        if language_code.trim().is_empty() {
            continue;
        }
        let name = raw
            .name
            .map(TextRuns::into_text)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| language_code.clone());
        tracks.push(CaptionTrack {
            base_url: validate_caption_url(&base_url)?,
            language_code,
            name,
            is_generated: raw.kind.as_deref() == Some("asr"),
        });
    }

    if tracks.is_empty() {
        return Err(Error::UpstreamLayout { service: "youtube" });
    }
    Ok(tracks)
}

pub fn select_caption_track<'a>(
    tracks: &'a [CaptionTrack],
    requested_language: Option<&str>,
) -> Option<&'a CaptionTrack> {
    if let Some(requested) = requested_language.filter(|language| !language.trim().is_empty()) {
        if let Some(track) = tracks
            .iter()
            .find(|track| track.language_code.eq_ignore_ascii_case(requested))
        {
            return Some(track);
        }
    }
    tracks
        .iter()
        .find(|track| track.language_code.eq_ignore_ascii_case("en"))
        .or_else(|| tracks.iter().find(|track| !track.is_generated))
        .or_else(|| tracks.iter().find(|track| track.is_generated))
        .or_else(|| tracks.first())
}

pub fn parse_json3_captions(bytes: &[u8]) -> CoreResult<String> {
    let document: Json3Captions = serde_json::from_slice(bytes).map_err(json_parse_error)?;
    let mut output = CaptionOutput::default();
    for event in document.events.into_iter().take(MAX_CAPTION_EVENTS) {
        let Some(text) = json3_event_text(event.segs) else {
            continue;
        };
        output.push(event.start_ms, event.duration_ms, &text)?;
    }
    Ok(output.finish())
}

pub fn parse_xml_captions(bytes: &[u8]) -> CoreResult<String> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut current: Option<XmlCaptionEvent> = None;
    let mut output = CaptionOutput::default();
    let mut events = 0usize;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) if start.local_name().as_ref() == b"text" => {
                current = Some(xml_caption_event(&start, reader.decoder())?);
            }
            Ok(Event::Empty(start)) if start.local_name().as_ref() == b"text" => {
                let event = xml_caption_event(&start, reader.decoder())?;
                output.push(event.start_ms, event.duration_ms, "")?;
            }
            Ok(Event::Text(text)) => {
                if let Some(event) = &mut current {
                    event
                        .text
                        .push_str(&text.decode().map_err(xml_parse_error)?);
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(event) = &mut current {
                    event
                        .text
                        .push_str(&text.decode().map_err(xml_parse_error)?);
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(event) = &mut current {
                    event.text.push_str(&decode_xml_reference(&reference)?);
                }
            }
            Ok(Event::End(end)) if end.local_name().as_ref() == b"text" => {
                if let Some(event) = current.take() {
                    events += 1;
                    if events > MAX_CAPTION_EVENTS {
                        break;
                    }
                    if let Some(text) = normalize_caption_text(&decode_entities(&event.text)) {
                        output.push(event.start_ms, event.duration_ms, &text)?;
                    }
                }
            }
            Ok(Event::DocType(_)) => {
                return Err(Error::Parse {
                    kind: "youtube captions",
                    message: "caption DTDs are not supported".into(),
                });
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(xml_parse_error(error)),
        }
        buffer.clear();
    }

    Ok(output.finish())
}

pub fn parse_search_results(html: &str, limit: usize) -> CoreResult<Vec<YoutubeSearchResult>> {
    let value = extract_named_json(html, &["ytInitialData"], value_has_search_results)?;
    let limit = limit.min(MAX_SEARCH_RESULTS);
    let mut budget = JsonTraversalBudget::default();
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    collect_video_renderers(&value, limit, &mut budget, &mut seen, &mut results)?;
    Ok(results)
}

#[derive(Debug, Deserialize)]
struct PlayerResponse {
    captions: Option<PlayerCaptions>,
}

#[derive(Debug, Deserialize)]
struct PlayerCaptions {
    #[serde(rename = "playerCaptionsTracklistRenderer")]
    player_captions_tracklist_renderer: Option<PlayerCaptionsTracklistRenderer>,
}

#[derive(Debug, Deserialize)]
struct PlayerCaptionsTracklistRenderer {
    #[serde(rename = "captionTracks", default)]
    caption_tracks: Vec<RawCaptionTrack>,
}

#[derive(Debug, Deserialize)]
struct RawCaptionTrack {
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
    #[serde(rename = "languageCode")]
    language_code: Option<String>,
    name: Option<TextRuns>,
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TextRuns {
    #[serde(rename = "simpleText")]
    simple_text: Option<String>,
    runs: Option<Vec<TextRun>>,
}

impl TextRuns {
    fn into_text(self) -> String {
        self.simple_text.unwrap_or_else(|| {
            self.runs
                .unwrap_or_default()
                .into_iter()
                .filter_map(|run| run.text)
                .collect()
        })
    }
}

#[derive(Debug, Deserialize)]
struct TextRun {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Json3Captions {
    #[serde(default)]
    events: Vec<Json3Event>,
}

#[derive(Debug, Deserialize)]
struct Json3Event {
    #[serde(rename = "tStartMs")]
    start_ms: Option<i64>,
    #[serde(rename = "dDurationMs")]
    duration_ms: Option<i64>,
    segs: Option<Vec<Json3Segment>>,
}

#[derive(Debug, Deserialize)]
struct Json3Segment {
    utf8: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawVideoRenderer {
    #[serde(rename = "videoId")]
    video_id: Option<String>,
    title: Option<TextRuns>,
}

#[derive(Default)]
struct CaptionOutput {
    text: String,
    previous_end_ms: Option<i64>,
}

impl CaptionOutput {
    fn push(
        &mut self,
        start_ms: Option<i64>,
        duration_ms: Option<i64>,
        text: &str,
    ) -> CoreResult<()> {
        let Some(text) = normalize_caption_text(text) else {
            return Ok(());
        };
        if !self.text.is_empty() {
            let start_ms = start_ms.unwrap_or_else(|| self.previous_end_ms.unwrap_or_default());
            if self
                .previous_end_ms
                .is_some_and(|previous| start_ms.saturating_sub(previous) >= CAPTION_GAP_MS)
            {
                push_bounded(&mut self.text, "\n\n")?;
            } else {
                push_bounded(&mut self.text, " ")?;
            }
        }
        push_bounded(&mut self.text, &text)?;
        if let Some(start_ms) = start_ms {
            let end_ms = start_ms.saturating_add(duration_ms.unwrap_or_default());
            self.previous_end_ms = Some(end_ms.max(start_ms));
        }
        Ok(())
    }

    fn finish(self) -> String {
        self.text
    }
}

struct XmlCaptionEvent {
    start_ms: Option<i64>,
    duration_ms: Option<i64>,
    text: String,
}

#[derive(Default)]
struct JsonTraversalBudget {
    visited: usize,
}

fn collect_video_renderers(
    value: &Value,
    limit: usize,
    budget: &mut JsonTraversalBudget,
    seen: &mut HashSet<String>,
    results: &mut Vec<YoutubeSearchResult>,
) -> CoreResult<()> {
    if results.len() >= limit {
        return Ok(());
    }
    budget.visited += 1;
    if budget.visited > MAX_JSON_NODES {
        return Err(Error::BodyLimit {
            limit: MAX_JSON_NODES,
        });
    }

    match value {
        Value::Object(object) => {
            if let Some(renderer) = object.get("videoRenderer") {
                let raw: RawVideoRenderer =
                    serde_json::from_value(renderer.clone()).map_err(json_parse_error)?;
                if let (Some(video_id), Some(title)) = (raw.video_id, raw.title) {
                    if is_valid_video_id(&video_id) && seen.insert(video_id.clone()) {
                        results.push(YoutubeSearchResult {
                            title: title.into_text(),
                            url: format!("https://www.youtube.com/watch?v={video_id}"),
                        });
                    }
                }
            }
            for child in object.values() {
                collect_video_renderers(child, limit, budget, seen, results)?;
                if results.len() >= limit {
                    break;
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_video_renderers(child, limit, budget, seen, results)?;
                if results.len() >= limit {
                    break;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn extract_named_json<F>(html: &str, names: &[&str], accepts: F) -> CoreResult<Value>
where
    F: Fn(&Value) -> CoreResult<bool>,
{
    if is_youtube_consent_or_layout(html) {
        return Err(Error::UpstreamLayout { service: "youtube" });
    }

    let mut scanner = NamedJsonScanner::new(html, names);
    scanner.extract(accepts)
}

#[cfg(test)]
fn extract_named_json_with_stats<F>(
    html: &str,
    names: &[&str],
    accepts: F,
    stats: &mut NamedJsonScanStats,
) -> CoreResult<Value>
where
    F: Fn(&Value) -> CoreResult<bool>,
{
    if is_youtube_consent_or_layout(html) {
        return Err(Error::UpstreamLayout { service: "youtube" });
    }

    let mut scanner = NamedJsonScanner::with_stats(html, names, stats);
    scanner.extract(accepts)
}

#[cfg(test)]
#[derive(Default, Debug)]
struct NamedJsonScanStats {
    steps: usize,
    candidate_bytes: usize,
    candidates: usize,
}

struct NamedJsonScanner<'a, 'stats> {
    html: &'a str,
    names: &'a [&'a str],
    #[cfg(test)]
    stats: Option<&'stats mut NamedJsonScanStats>,
    #[cfg(not(test))]
    _stats: std::marker::PhantomData<&'stats ()>,
}

impl<'a> NamedJsonScanner<'a, '_> {
    fn new(html: &'a str, names: &'a [&'a str]) -> Self {
        Self {
            html,
            names,
            #[cfg(test)]
            stats: None,
            #[cfg(not(test))]
            _stats: std::marker::PhantomData,
        }
    }
}

#[cfg(test)]
impl<'a, 'stats> NamedJsonScanner<'a, 'stats> {
    fn with_stats(
        html: &'a str,
        names: &'a [&'a str],
        stats: &'stats mut NamedJsonScanStats,
    ) -> Self {
        Self {
            html,
            names,
            stats: Some(stats),
        }
    }
}

impl<'a, 'stats> NamedJsonScanner<'a, 'stats> {
    fn extract<F>(&mut self, accepts: F) -> CoreResult<Value>
    where
        F: Fn(&Value) -> CoreResult<bool>,
    {
        let mut first_parseable: Option<(usize, Value)> = None;
        let mut first_accepted: Option<(usize, Value)> = None;
        let mut index = 0usize;

        while index < self.html.len() {
            match self.scan_at(index) {
                NamedJsonScan::Candidate {
                    raw,
                    end,
                    name_priority,
                } => {
                    index = end.max(index + 1).min(self.html.len());
                    let Ok(value) = raw_to_json(raw) else {
                        continue;
                    };
                    if first_parseable
                        .as_ref()
                        .is_none_or(|(priority, _)| name_priority < *priority)
                    {
                        first_parseable = Some((name_priority, value.clone()));
                    }
                    if accepts(&value)? {
                        if name_priority == 0 {
                            return Ok(value);
                        }
                        if first_accepted
                            .as_ref()
                            .is_none_or(|(priority, _)| name_priority < *priority)
                        {
                            first_accepted = Some((name_priority, value));
                        }
                    }
                }
                NamedJsonScan::Advance(next) => {
                    index = next.max(index + 1).min(self.html.len());
                }
            }
        }

        first_accepted
            .map(|(_, value)| value)
            .or_else(|| first_parseable.map(|(_, value)| value))
            .ok_or(Error::UpstreamLayout { service: "youtube" })
    }

    fn scan_at(&mut self, index: usize) -> NamedJsonScan<'a> {
        if self.starts_with_at(index, "<!--") {
            return NamedJsonScan::Advance(self.skip_line_comment(index));
        }
        if self.starts_with_at(index, "-->") {
            return NamedJsonScan::Advance(self.skip_line_comment(index));
        }
        if self.starts_with_at(index, "//") {
            return NamedJsonScan::Advance(self.skip_line_comment(index));
        }
        if self.starts_with_at(index, "/*") {
            return NamedJsonScan::Advance(self.skip_block_comment(index));
        }

        match self.byte_at(index) {
            Some(b'"' | b'\'') => NamedJsonScan::Advance(self.skip_js_string(index)),
            Some(b'`') => NamedJsonScan::Advance(self.skip_template_literal(index)),
            Some(b'[') => self.scan_bracket_property(index),
            Some(b'.') => self.scan_dot_property(index),
            Some(_) => self.scan_bare_name(index),
            None => NamedJsonScan::Advance(self.html.len()),
        }
    }

    fn scan_bare_name(&mut self, index: usize) -> NamedJsonScan<'a> {
        let Some((priority, name)) = self.match_bare_name(index) else {
            return NamedJsonScan::Advance(self.advance_char(index));
        };
        self.scan_after_lhs(index + name.len(), priority)
    }

    fn scan_dot_property(&mut self, index: usize) -> NamedJsonScan<'a> {
        let name_start = self.skip_ws_and_comments(index + 1);
        let Some((priority, name)) = self.match_property_name(name_start) else {
            return NamedJsonScan::Advance(name_start);
        };
        self.scan_after_lhs(name_start + name.len(), priority)
    }

    fn scan_bracket_property(&mut self, index: usize) -> NamedJsonScan<'a> {
        let name_start = self.skip_ws_and_comments(index + 1);
        let Some(quote) = self
            .byte_at(name_start)
            .filter(|byte| matches!(byte, b'"' | b'\''))
        else {
            return NamedJsonScan::Advance(name_start);
        };
        let Some((property_name, after_name)) = self.read_js_string(name_start, quote) else {
            return NamedJsonScan::Advance(self.html.len());
        };
        let Some(priority) = self.name_priority(&property_name) else {
            return NamedJsonScan::Advance(after_name);
        };
        let after_name = self.skip_ws_and_comments(after_name);
        if self.byte_at(after_name) != Some(b']') {
            return NamedJsonScan::Advance(after_name);
        }
        self.scan_after_lhs(after_name + 1, priority)
    }

    fn scan_after_lhs(&mut self, after_lhs: usize, name_priority: usize) -> NamedJsonScan<'a> {
        let separator = self.skip_ws_and_comments(after_lhs);
        if self.byte_at(separator) != Some(b'=') {
            return NamedJsonScan::Advance(separator);
        }
        let value_start = self.skip_ws_and_comments(separator + 1);
        match self.byte_at(value_start) {
            Some(b'{') => self.scan_json_object(value_start, name_priority),
            Some(b'"') => self.scan_json_string_candidate(value_start, name_priority),
            _ => NamedJsonScan::Advance(value_start),
        }
    }

    fn scan_json_object(&mut self, start: usize, name_priority: usize) -> NamedJsonScan<'a> {
        self.count_candidate();
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut index = start;

        while index < self.html.len() {
            let Some(ch) = self.char_at(index) else {
                break;
            };
            let next = index + ch.len_utf8();
            self.count_step(ch.len_utf8());
            self.count_candidate_bytes(ch.len_utf8());

            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                index = next;
                continue;
            }

            match ch {
                '"' => in_string = true,
                '{' => depth = depth.saturating_add(1),
                '}' => {
                    let Some(next_depth) = depth.checked_sub(1) else {
                        return NamedJsonScan::Advance(next);
                    };
                    depth = next_depth;
                    if depth == 0 {
                        return NamedJsonScan::Candidate {
                            raw: &self.html[start..next],
                            end: next,
                            name_priority,
                        };
                    }
                }
                _ => {}
            }
            index = next;
        }

        NamedJsonScan::Advance(self.html.len())
    }

    fn scan_json_string_candidate(
        &mut self,
        start: usize,
        name_priority: usize,
    ) -> NamedJsonScan<'a> {
        self.count_candidate();
        self.count_step(1);
        self.count_candidate_bytes(1);
        let mut escaped = false;
        let mut index = start + 1;

        while index < self.html.len() {
            let Some(ch) = self.char_at(index) else {
                break;
            };
            let next = index + ch.len_utf8();
            self.count_step(ch.len_utf8());
            self.count_candidate_bytes(ch.len_utf8());
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                return NamedJsonScan::Candidate {
                    raw: &self.html[start..next],
                    end: next,
                    name_priority,
                };
            }
            index = next;
        }

        NamedJsonScan::Advance(self.html.len())
    }

    fn skip_ws_and_comments(&mut self, mut index: usize) -> usize {
        loop {
            while let Some(ch) = self.char_at(index).filter(|ch| ch.is_whitespace()) {
                self.count_step(ch.len_utf8());
                index += ch.len_utf8();
            }
            if self.starts_with_at(index, "//") {
                index = self.skip_line_comment(index);
            } else if self.starts_with_at(index, "/*") {
                index = self.skip_block_comment(index);
            } else if self.starts_with_at(index, "<!--") || self.starts_with_at(index, "-->") {
                index = self.skip_line_comment(index);
            } else {
                return index;
            }
        }
    }

    fn skip_line_comment(&mut self, index: usize) -> usize {
        let mut cursor = index;
        while cursor < self.html.len() {
            let Some(ch) = self.char_at(cursor) else {
                break;
            };
            let next = cursor + ch.len_utf8();
            self.count_step(ch.len_utf8());
            cursor = next;
            if is_javascript_line_terminator(ch) {
                break;
            }
        }
        cursor
    }

    fn skip_block_comment(&mut self, index: usize) -> usize {
        let bytes = self.html.as_bytes();
        let mut cursor = index;
        while cursor < bytes.len() {
            self.count_step(1);
            cursor += 1;
            if cursor >= index + 2 && bytes.get(cursor - 2..cursor) == Some(b"*/") {
                break;
            }
        }
        cursor
    }

    fn skip_js_string(&mut self, index: usize) -> usize {
        let Some(quote) = self.byte_at(index) else {
            return self.html.len();
        };
        self.count_step(1);
        let mut escaped = false;
        let mut cursor = index + 1;
        while cursor < self.html.len() {
            let Some(ch) = self.char_at(cursor) else {
                break;
            };
            let next = cursor + ch.len_utf8();
            self.count_step(ch.len_utf8());
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == char::from(quote) {
                return next;
            }
            cursor = next;
        }
        self.html.len()
    }

    fn skip_template_literal(&mut self, index: usize) -> usize {
        self.count_step(1);
        let mut states = vec![TemplateLiteralState::Raw];
        let mut cursor = index + 1;

        while cursor < self.html.len() {
            let Some(state) = states.last().copied() else {
                return cursor;
            };

            match state {
                TemplateLiteralState::Raw => {
                    if self.starts_with_at(cursor, "${") {
                        self.count_step(2);
                        cursor += 2;
                        *states.last_mut().expect("template state exists") =
                            TemplateLiteralState::Interpolation {
                                brace_depth: 1,
                                can_start_regex: true,
                                property_name_expected: false,
                            };
                        continue;
                    }

                    let Some(ch) = self.char_at(cursor) else {
                        break;
                    };
                    self.count_step(ch.len_utf8());
                    cursor += ch.len_utf8();

                    if ch == '\\' {
                        if let Some(escaped) = self.char_at(cursor) {
                            self.count_step(escaped.len_utf8());
                            cursor += escaped.len_utf8();
                        }
                    } else if ch == '`' {
                        states.pop();
                        if states.is_empty() {
                            return cursor;
                        }
                    }
                }
                TemplateLiteralState::Interpolation {
                    brace_depth,
                    can_start_regex,
                    property_name_expected,
                } => {
                    if self.starts_with_at(cursor, "<!--")
                        || self.starts_with_at(cursor, "-->")
                        || self.starts_with_at(cursor, "//")
                    {
                        cursor = self.skip_line_comment(cursor);
                        continue;
                    }
                    if self.starts_with_at(cursor, "/*") {
                        cursor = self.skip_block_comment(cursor);
                        continue;
                    }

                    match self.byte_at(cursor) {
                        Some(b'"' | b'\'') => {
                            cursor = self.skip_js_string(cursor);
                            *states.last_mut().expect("template state exists") =
                                TemplateLiteralState::Interpolation {
                                    brace_depth,
                                    can_start_regex: false,
                                    property_name_expected: false,
                                };
                        }
                        Some(b'`') => {
                            self.count_step(1);
                            cursor += 1;
                            *states.last_mut().expect("template state exists") =
                                TemplateLiteralState::Interpolation {
                                    brace_depth,
                                    can_start_regex: false,
                                    property_name_expected: false,
                                };
                            states.push(TemplateLiteralState::Raw);
                        }
                        Some(b'/') if can_start_regex => {
                            cursor = self.skip_js_regex_literal(cursor);
                            *states.last_mut().expect("template state exists") =
                                TemplateLiteralState::Interpolation {
                                    brace_depth,
                                    can_start_regex: false,
                                    property_name_expected: false,
                                };
                        }
                        Some(b'/') => {
                            self.count_step(1);
                            cursor += 1;
                            if self.byte_at(cursor) == Some(b'=') {
                                self.count_step(1);
                                cursor += 1;
                            }
                            *states.last_mut().expect("template state exists") =
                                TemplateLiteralState::Interpolation {
                                    brace_depth,
                                    can_start_regex: true,
                                    property_name_expected: false,
                                };
                        }
                        Some(b'.') if self.starts_with_at(cursor, "...") => {
                            self.count_step(3);
                            cursor += 3;
                            *states.last_mut().expect("template state exists") =
                                TemplateLiteralState::Interpolation {
                                    brace_depth,
                                    can_start_regex: true,
                                    property_name_expected: false,
                                };
                        }
                        Some(_) => {
                            let Some(ch) = self.char_at(cursor) else {
                                break;
                            };

                            if is_javascript_identifier_start(ch) {
                                let start = cursor;
                                cursor = self.skip_javascript_identifier(cursor);
                                *states.last_mut().expect("template state exists") =
                                    TemplateLiteralState::Interpolation {
                                        brace_depth,
                                        can_start_regex: !property_name_expected
                                            && javascript_keyword_allows_regex_after(
                                                &self.html[start..cursor],
                                            ),
                                        property_name_expected: false,
                                    };
                                continue;
                            }

                            self.count_step(ch.len_utf8());
                            cursor += ch.len_utf8();

                            if ch == '{' {
                                *states.last_mut().expect("template state exists") =
                                    TemplateLiteralState::Interpolation {
                                        brace_depth: brace_depth.saturating_add(1),
                                        can_start_regex: true,
                                        property_name_expected: false,
                                    };
                            } else if ch == '}' {
                                *states.last_mut().expect("template state exists") =
                                    if brace_depth == 1 {
                                        TemplateLiteralState::Raw
                                    } else {
                                        TemplateLiteralState::Interpolation {
                                            brace_depth: brace_depth - 1,
                                            can_start_regex: false,
                                            property_name_expected: false,
                                        }
                                    };
                            } else {
                                let (next_can_start_regex, next_property_name_expected) = match ch {
                                    ch if ch.is_whitespace() => {
                                        (can_start_regex, property_name_expected)
                                    }
                                    ch if ch.is_ascii_digit() => (false, false),
                                    '(' | '[' | ',' | ';' | ':' | '?' => (true, false),
                                    ')' | ']' => (false, false),
                                    '.' => (false, true),
                                    '+' | '-' if self.byte_at(cursor) == Some(ch as u8) => {
                                        self.count_step(1);
                                        cursor += 1;
                                        (can_start_regex, false)
                                    }
                                    '+' | '-' | '*' | '%' | '&' | '|' | '^' | '!' | '~' | '<'
                                    | '>' | '=' => (true, false),
                                    _ => (true, false),
                                };
                                *states.last_mut().expect("template state exists") =
                                    TemplateLiteralState::Interpolation {
                                        brace_depth,
                                        can_start_regex: next_can_start_regex,
                                        property_name_expected: next_property_name_expected,
                                    };
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        self.html.len()
    }

    fn skip_js_regex_literal(&mut self, index: usize) -> usize {
        self.count_step(1);
        let mut cursor = index + 1;
        let mut escaped = false;
        let mut in_character_class = false;

        while cursor < self.html.len() {
            let Some(ch) = self.char_at(cursor) else {
                break;
            };
            self.count_step(ch.len_utf8());
            cursor += ch.len_utf8();

            if is_javascript_line_terminator(ch) {
                return self.html.len();
            }
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if in_character_class {
                if ch == ']' {
                    in_character_class = false;
                }
                continue;
            }
            if ch == '[' {
                in_character_class = true;
            } else if ch == '/' {
                while let Some(flag) = self
                    .char_at(cursor)
                    .filter(|flag| is_javascript_identifier_continue(*flag))
                {
                    self.count_step(flag.len_utf8());
                    cursor += flag.len_utf8();
                }
                return cursor;
            }
        }

        self.html.len()
    }

    fn skip_javascript_identifier(&mut self, index: usize) -> usize {
        let mut cursor = index;
        while let Some(ch) = self
            .char_at(cursor)
            .filter(|ch| is_javascript_identifier_continue(*ch))
        {
            self.count_step(ch.len_utf8());
            cursor += ch.len_utf8();
        }
        cursor
    }

    fn read_js_string(&mut self, index: usize, quote: u8) -> Option<(String, usize)> {
        self.count_step(1);
        let mut escaped = false;
        let mut cursor = index + 1;
        let mut value = String::new();
        while cursor < self.html.len() {
            let ch = self.char_at(cursor)?;
            let next = cursor + ch.len_utf8();
            self.count_step(ch.len_utf8());
            if escaped {
                value.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == char::from(quote) {
                return Some((value, next));
            } else {
                value.push(ch);
            }
            cursor = next;
        }
        None
    }

    fn match_bare_name(&self, index: usize) -> Option<(usize, &'a str)> {
        if self.previous_token_continues_identifier(index) {
            return None;
        }
        self.match_property_name(index)
    }

    fn match_property_name(&self, index: usize) -> Option<(usize, &'a str)> {
        self.names
            .iter()
            .copied()
            .enumerate()
            .find(|(_, name)| self.name_matches_at(index, name))
    }

    fn name_matches_at(&self, index: usize, name: &str) -> bool {
        self.html
            .get(index..)
            .is_some_and(|tail| tail.starts_with(name))
            && !self.next_token_continues_identifier(index + name.len())
    }

    fn name_priority(&self, candidate: &str) -> Option<usize> {
        self.names.iter().position(|name| *name == candidate)
    }

    fn previous_token_continues_identifier(&self, index: usize) -> bool {
        self.previous_char(index)
            .is_some_and(is_javascript_identifier_continue)
            || self
                .html
                .as_bytes()
                .get(..index)
                .is_some_and(source_ends_with_identifier_escape)
    }

    fn next_token_continues_identifier(&self, index: usize) -> bool {
        self.char_at(index)
            .is_some_and(is_javascript_identifier_continue)
            || self
                .html
                .as_bytes()
                .get(index..)
                .is_some_and(source_starts_with_identifier_escape)
    }

    fn starts_with_at(&self, index: usize, needle: &str) -> bool {
        self.html
            .as_bytes()
            .get(index..)
            .is_some_and(|tail| tail.starts_with(needle.as_bytes()))
    }

    fn byte_at(&self, index: usize) -> Option<u8> {
        self.html.as_bytes().get(index).copied()
    }

    fn char_at(&self, index: usize) -> Option<char> {
        self.html.get(index..)?.chars().next()
    }

    fn previous_char(&self, index: usize) -> Option<char> {
        self.html.get(..index)?.chars().next_back()
    }

    fn advance_char(&mut self, index: usize) -> usize {
        let Some(ch) = self.char_at(index) else {
            return self.html.len();
        };
        self.count_step(ch.len_utf8());
        index + ch.len_utf8()
    }

    fn count_step(&mut self, bytes: usize) {
        #[cfg(test)]
        if let Some(stats) = self.stats.as_deref_mut() {
            stats.steps = stats.steps.saturating_add(bytes);
        }
        let _ = bytes;
    }

    fn count_candidate(&mut self) {
        #[cfg(test)]
        if let Some(stats) = self.stats.as_deref_mut() {
            stats.candidates = stats.candidates.saturating_add(1);
        }
    }

    fn count_candidate_bytes(&mut self, bytes: usize) {
        #[cfg(test)]
        if let Some(stats) = self.stats.as_deref_mut() {
            stats.candidate_bytes = stats.candidate_bytes.saturating_add(bytes);
        }
        let _ = bytes;
    }
}

enum NamedJsonScan<'a> {
    Candidate {
        raw: &'a str,
        end: usize,
        name_priority: usize,
    },
    Advance(usize),
}

#[derive(Clone, Copy)]
enum TemplateLiteralState {
    Raw,
    Interpolation {
        brace_depth: usize,
        can_start_regex: bool,
        property_name_expected: bool,
    },
}

fn is_javascript_identifier_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == '$' || (!ch.is_ascii() && !ch.is_whitespace())
}

fn is_javascript_identifier_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || (!ch.is_ascii() && !ch.is_whitespace())
}

fn javascript_keyword_allows_regex_after(token: &str) -> bool {
    matches!(
        token,
        "await"
            | "case"
            | "delete"
            | "do"
            | "else"
            | "in"
            | "instanceof"
            | "new"
            | "of"
            | "return"
            | "throw"
            | "typeof"
            | "void"
            | "yield"
    )
}

fn is_javascript_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn source_ends_with_identifier_escape(bytes: &[u8]) -> bool {
    if bytes.len() >= 6 {
        let start = bytes.len() - 6;
        if bytes[start] == b'\\'
            && bytes[start + 1] == b'u'
            && bytes[start + 2..]
                .iter()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return true;
        }
    }

    if bytes.last() != Some(&b'}') {
        return false;
    }
    let close = bytes.len() - 1;
    let mut first_digit = close;
    while first_digit > 0 && bytes[first_digit - 1].is_ascii_hexdigit() {
        first_digit -= 1;
    }
    first_digit < close
        && first_digit >= 3
        && bytes[first_digit - 1] == b'{'
        && bytes[first_digit - 2] == b'u'
        && bytes[first_digit - 3] == b'\\'
}

fn source_starts_with_identifier_escape(bytes: &[u8]) -> bool {
    if bytes.len() >= 6
        && bytes[0] == b'\\'
        && bytes[1] == b'u'
        && bytes[2..6].iter().all(|byte| byte.is_ascii_hexdigit())
    {
        return true;
    }

    if !bytes.starts_with(b"\\u{") {
        return false;
    }
    let mut cursor = 3usize;
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_hexdigit())
    {
        cursor += 1;
    }
    cursor > 3 && bytes.get(cursor) == Some(&b'}')
}

fn value_has_caption_tracks(value: &Value) -> CoreResult<bool> {
    Ok(value
        .pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")
        .and_then(Value::as_array)
        .is_some_and(|tracks| !tracks.is_empty()))
}

fn value_has_search_results(value: &Value) -> CoreResult<bool> {
    let mut budget = JsonTraversalBudget::default();
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    collect_video_renderers(value, 1, &mut budget, &mut seen, &mut results)?;
    Ok(!results.is_empty())
}

fn raw_to_json(raw: &str) -> CoreResult<Value> {
    if raw.trim_start().starts_with('{') {
        serde_json::from_str(raw).map_err(json_parse_error)
    } else {
        let decoded: String = serde_json::from_str(raw).map_err(json_parse_error)?;
        serde_json::from_str(&decoded).map_err(json_parse_error)
    }
}

fn validate_caption_url(candidate: &str) -> CoreResult<Url> {
    let url = Url::parse(candidate).map_err(|_| Error::Policy("caption URL is invalid".into()))?;
    validate_caption_url_parts(&url)?;
    validate_nested_caption_urls(&url, 0)?;
    Ok(url)
}

fn validate_caption_url_parts(url: &Url) -> CoreResult<()> {
    if url.scheme() != "https" {
        return Err(Error::Policy("caption URL must use HTTPS".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Policy(
            "caption URL credentials are not allowed".into(),
        ));
    }
    if url.port_or_known_default() != Some(443) {
        return Err(Error::Policy(
            "caption URL must use the default HTTPS port".into(),
        ));
    }
    if !is_allowed_youtube_or_google_host(url) {
        return Err(Error::Policy("caption URL host is not allowed".into()));
    }
    Ok(())
}

fn validate_nested_caption_urls(url: &Url, depth: usize) -> CoreResult<()> {
    for (_, value) in url.query_pairs() {
        validate_nested_caption_value(&value, depth)?;
    }
    Ok(())
}

fn validate_nested_caption_value(value: &str, depth: usize) -> CoreResult<()> {
    if value.len() > MAX_NESTED_URL_VALUE_BYTES {
        return Err(Error::Policy(
            "caption URL nested value exceeds the inspection limit".into(),
        ));
    }
    if depth > MAX_NESTED_URL_DECODE_DEPTH {
        return Err(Error::Policy("caption URL nesting is too deep".into()));
    }
    if let Ok(nested) = Url::parse(value) {
        validate_caption_url_parts(&nested)
            .map_err(|_| Error::Policy("caption URL contains an unsafe nested URL".into()))?;
        validate_nested_caption_urls(&nested, depth + 1)?;
        return Ok(());
    }
    let Some(decoded) = percent_decode_once(value)? else {
        return Ok(());
    };
    validate_nested_caption_value(&decoded, depth + 1)
}

fn percent_decode_once(input: &str) -> CoreResult<Option<String>> {
    if !input.as_bytes().contains(&b'%') {
        return Ok(None);
    }

    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                output.push((high << 4) | low);
                index += 3;
                changed = true;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }

    if !changed {
        return Ok(None);
    }
    String::from_utf8(output)
        .map(Some)
        .map_err(|_| Error::Policy("caption URL nested value is invalid".into()))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn caption_host_restriction() -> CoreResult<FetchHostRestriction> {
    FetchHostRestriction::https_label_suffixes(CAPTION_ALLOWED_HOST_SUFFIXES.iter().copied())
}

fn is_allowed_youtube_or_google_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    CAPTION_ALLOWED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

fn with_json3_format(mut url: Url) -> Url {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| name != "fmt")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in pairs {
            query.append_pair(&name, &value);
        }
        query.append_pair("fmt", "json3");
    }
    url
}

fn json3_event_text(segs: Option<Vec<Json3Segment>>) -> Option<String> {
    let mut text = String::new();
    for segment in segs? {
        if let Some(utf8) = segment.utf8 {
            text.push_str(&utf8);
        }
    }
    normalize_caption_text(&decode_entities(&text))
}

fn xml_caption_event(start: &BytesStart<'_>, decoder: Decoder) -> CoreResult<XmlCaptionEvent> {
    let mut start_ms = None;
    let mut duration_ms = None;
    for attribute in start.attributes() {
        let attribute = attribute.map_err(xml_parse_error)?;
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, decoder)
            .map_err(xml_parse_error)?;
        match attribute.key.as_ref() {
            b"start" => start_ms = seconds_to_ms(&value),
            b"dur" => duration_ms = seconds_to_ms(&value),
            _ => {}
        }
    }
    Ok(XmlCaptionEvent {
        start_ms,
        duration_ms,
        text: String::new(),
    })
}

fn seconds_to_ms(value: &str) -> Option<i64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole_ms = whole.parse::<i64>().ok()?.checked_mul(1_000)?;
    let mut fraction_digits = fraction
        .chars()
        .take(3)
        .filter_map(|ch| ch.to_digit(10))
        .collect::<Vec<_>>();
    while fraction_digits.len() < 3 {
        fraction_digits.push(0);
    }
    let fraction_ms = fraction_digits.into_iter().fold(0i64, |acc, digit| {
        acc.saturating_mul(10).saturating_add(i64::from(digit))
    });
    whole_ms.checked_add(fraction_ms)
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

fn decode_entities(input: &str) -> String {
    quick_xml::escape::unescape(input)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| input.to_string())
}

fn normalize_caption_text(input: &str) -> Option<String> {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded = truncate_chars(&normalized, MAX_CAPTION_CHARS);
    if bounded.is_empty() {
        None
    } else {
        Some(bounded)
    }
}

fn push_bounded(target: &mut String, value: &str) -> CoreResult<()> {
    if target.chars().count() + value.chars().count() > MAX_CAPTION_CHARS {
        return Err(Error::BodyLimit {
            limit: MAX_CAPTION_CHARS,
        });
    }
    target.push_str(value);
    Ok(())
}

fn is_valid_video_id(id: &str) -> bool {
    id.len() == VIDEO_ID_LEN && id.chars().all(is_video_id_char)
}

fn is_video_id_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
}

fn is_youtube_consent_or_layout(html: &str) -> bool {
    html.contains("consent.youtube.com")
        || html.contains("ServiceLogin")
        || html.contains("captcha")
        || html.contains("unusual traffic")
}

fn json_parse_error(error: serde_json::Error) -> Error {
    Error::Parse {
        kind: "youtube json",
        message: error.to_string(),
    }
}

fn xml_parse_error(error: impl std::fmt::Display) -> Error {
    Error::Parse {
        kind: "youtube captions",
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_truncated_state_candidates_are_scanned_once() {
        let html = "ytInitialPlayerResponse = {".repeat(2_000);
        let mut stats = NamedJsonScanStats::default();

        let error = extract_named_json_with_stats(
            &html,
            &["ytInitialPlayerResponse"],
            value_has_caption_tracks,
            &mut stats,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            Error::UpstreamLayout { service: "youtube" }
        ));
        assert_eq!(stats.candidates, 1);
        assert!(stats.candidate_bytes <= html.len());
        assert!(stats.steps <= html.len().saturating_mul(3));
    }

    #[test]
    fn many_wrong_shape_state_candidates_are_scanned_linearly() {
        let mut html = String::new();
        for _ in 0..1_000 {
            html.push_str(r#"ytInitialPlayerResponse = {"notCaptions":true};"#);
        }
        html.push_str(&format!(
            "window.ytInitialPlayerResponse = {};",
            test_player_object("en", "Real English")
        ));
        let mut stats = NamedJsonScanStats::default();

        let value = extract_named_json_with_stats(
            &html,
            &["ytInitialPlayerResponse"],
            value_has_caption_tracks,
            &mut stats,
        )
        .unwrap();

        assert!(value_has_caption_tracks(&value).unwrap());
        assert_eq!(stats.candidates, 1_001);
        assert!(stats.candidate_bytes <= html.len());
        assert!(stats.steps <= html.len().saturating_mul(4));
    }

    #[test]
    fn unterminated_template_literal_is_skipped_to_end_linearly() {
        let html = format!("`{}", "ytInitialPlayerResponse = {".repeat(2_000));
        let mut stats = NamedJsonScanStats::default();

        let error = extract_named_json_with_stats(
            &html,
            &["ytInitialPlayerResponse"],
            value_has_caption_tracks,
            &mut stats,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            Error::UpstreamLayout { service: "youtube" }
        ));
        assert_eq!(stats.candidates, 0);
        assert_eq!(stats.candidate_bytes, 0);
        assert!(stats.steps <= html.len().saturating_mul(2));
    }

    #[test]
    fn deeply_nested_template_literals_are_skipped_iteratively_and_linearly() {
        let depth = 10_000;
        let fake = test_player_object("de", "Nested Template German");
        let real = test_player_object("en", "Real English");
        let mut html = String::from("`");
        for _ in 1..depth {
            html.push_str("${`");
        }
        html.push_str("ytInitialPlayerResponse = ");
        html.push_str(&fake);
        html.push(';');
        for _ in 1..depth {
            html.push_str("`}");
        }
        html.push('`');
        html.push_str("; ytInitialPlayerResponse = ");
        html.push_str(&real);
        html.push(';');
        let mut stats = NamedJsonScanStats::default();

        let value = extract_named_json_with_stats(
            &html,
            &["ytInitialPlayerResponse"],
            value_has_caption_tracks,
            &mut stats,
        )
        .unwrap();

        assert_eq!(
            value.pointer(
                "/captions/playerCaptionsTracklistRenderer/captionTracks/0/name/simpleText"
            ),
            Some(&Value::String("Real English".to_string()))
        );
        assert_eq!(stats.candidates, 1);
        assert!(stats.candidate_bytes <= html.len());
        assert!(stats.steps <= html.len().saturating_mul(3));
    }

    #[test]
    fn unterminated_deep_template_nesting_is_skipped_to_end_linearly() {
        let depth = 10_000;
        let mut html = String::from("`");
        for _ in 1..depth {
            html.push_str("${`");
        }
        html.push_str("ytInitialPlayerResponse = ");
        html.push_str(&test_player_object("de", "Nested Template German"));
        html.push(';');
        let mut stats = NamedJsonScanStats::default();

        let error = extract_named_json_with_stats(
            &html,
            &["ytInitialPlayerResponse"],
            value_has_caption_tracks,
            &mut stats,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            Error::UpstreamLayout { service: "youtube" }
        ));
        assert_eq!(stats.candidates, 0);
        assert_eq!(stats.candidate_bytes, 0);
        assert!(stats.steps <= html.len().saturating_mul(2));
    }

    #[test]
    fn large_nested_template_content_is_scanned_linearly() {
        let fake = test_player_object("de", "Nested Template German");
        let real = test_player_object("en", "Real English");
        let mut html = String::from("`${`");
        html.push_str("ytInitialPlayerResponse = ");
        html.push_str(&fake);
        html.push(';');
        html.push_str(&"template text ".repeat(40_000));
        html.push_str("`} outer`; ytInitialPlayerResponse = ");
        html.push_str(&real);
        html.push(';');
        let mut stats = NamedJsonScanStats::default();

        let value = extract_named_json_with_stats(
            &html,
            &["ytInitialPlayerResponse"],
            value_has_caption_tracks,
            &mut stats,
        )
        .unwrap();

        assert_eq!(
            value.pointer(
                "/captions/playerCaptionsTracklistRenderer/captionTracks/0/name/simpleText"
            ),
            Some(&Value::String("Real English".to_string()))
        );
        assert_eq!(stats.candidates, 1);
        assert!(stats.candidate_bytes <= html.len());
        assert!(stats.steps <= html.len().saturating_mul(3));
    }

    #[test]
    fn deeply_nested_template_regex_literals_are_scanned_linearly() {
        let depth = 5_000;
        let fake = test_player_object("de", "Nested Regex German");
        let real = test_player_object("en", "Real English");
        let mut html = String::from("`");
        for _ in 1..depth {
            html.push_str("${/[}`]/, `");
        }
        html.push_str("ytInitialPlayerResponse = ");
        html.push_str(&fake);
        html.push(';');
        for _ in 1..depth {
            html.push_str("`}");
        }
        html.push('`');
        html.push_str("; ytInitialPlayerResponse = ");
        html.push_str(&real);
        html.push(';');
        let mut stats = NamedJsonScanStats::default();

        let value = extract_named_json_with_stats(
            &html,
            &["ytInitialPlayerResponse"],
            value_has_caption_tracks,
            &mut stats,
        )
        .unwrap();

        assert_eq!(
            value.pointer(
                "/captions/playerCaptionsTracklistRenderer/captionTracks/0/name/simpleText"
            ),
            Some(&Value::String("Real English".to_string()))
        );
        assert_eq!(stats.candidates, 1);
        assert!(stats.candidate_bytes <= html.len());
        assert!(stats.steps <= html.len().saturating_mul(3));
    }

    #[test]
    fn large_repeated_template_regex_literals_are_scanned_linearly() {
        let fake = test_player_object("de", "Repeated Regex German");
        let real = test_player_object("en", "Real English");
        let mut html = String::from("`${");
        html.push_str(&"/[}`/]/g,".repeat(40_001));
        html.push_str("0} raw ytInitialPlayerResponse = ");
        html.push_str(&fake);
        html.push_str("; tail`; ytInitialPlayerResponse = ");
        html.push_str(&real);
        html.push(';');
        let mut stats = NamedJsonScanStats::default();

        let value = extract_named_json_with_stats(
            &html,
            &["ytInitialPlayerResponse"],
            value_has_caption_tracks,
            &mut stats,
        )
        .unwrap();

        assert_eq!(
            value.pointer(
                "/captions/playerCaptionsTracklistRenderer/captionTracks/0/name/simpleText"
            ),
            Some(&Value::String("Real English".to_string()))
        );
        assert_eq!(stats.candidates, 1);
        assert!(stats.candidate_bytes <= html.len());
        assert!(stats.steps <= html.len().saturating_mul(3));
    }

    #[test]
    fn unterminated_template_regex_literals_skip_to_input_end_linearly() {
        let fake = test_player_object("de", "Unterminated Regex German");
        let real = test_player_object("en", "Real English");

        for terminator in ["", "\n", "\r", "\u{2028}", "\u{2029}"] {
            let html = [
                "`${/[}`",
                terminator,
                " ytInitialPlayerResponse = ",
                &fake,
                "; raw`; ytInitialPlayerResponse = ",
                &real,
                ";",
            ]
            .concat();
            let mut stats = NamedJsonScanStats::default();

            let error = extract_named_json_with_stats(
                &html,
                &["ytInitialPlayerResponse"],
                value_has_caption_tracks,
                &mut stats,
            )
            .unwrap_err();

            assert!(
                matches!(error, Error::UpstreamLayout { service: "youtube" }),
                "terminator {terminator:?}"
            );
            assert_eq!(stats.candidates, 0, "terminator {terminator:?}");
            assert_eq!(stats.candidate_bytes, 0, "terminator {terminator:?}");
            assert!(
                stats.steps <= html.len().saturating_mul(2),
                "terminator {terminator:?}"
            );
        }
    }

    fn test_player_object(language_code: &str, name: &str) -> String {
        format!(
            r#"{{
              "captions": {{
                "playerCaptionsTracklistRenderer": {{
                  "captionTracks": [{{
                    "baseUrl": "https://www.youtube.com/api/timedtext?v=abc_def-123&lang={language_code}",
                    "languageCode": "{language_code}",
                    "name": {{"simpleText": "{name}"}}
                  }}]
                }}
              }}
            }}"#
        )
    }
}
