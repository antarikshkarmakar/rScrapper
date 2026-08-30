//! Typed web reading and search services.

use crate::context::AppContext;
use anyhow::Result as AnyResult;
use futures_util::{stream, StreamExt};
use rscraper_core::markdown::{html_to_markdown_with_options, MarkdownOptions};
use rscraper_core::{truncate_chars, Error, FetchRequest, FetchVia, Result};
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use url::Url;

pub const DEFAULT_SEARCH_RESULTS: usize = 5;
pub const MAX_SEARCH_RESULTS: usize = 20;
const MAX_QUERY_CHARS: usize = 1_024;
const MAX_TITLE_CHARS: usize = 512;
const MAX_SNIPPET_CHARS: usize = 4_096;
const SCRAPE_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Serialize)]
pub struct ReadResponse {
    pub url: Url,
    pub status: u16,
    pub via: &'static str,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub title: String,
    pub url: Url,
    pub snippet: String,
    pub markdown: Option<String>,
    pub scrape_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub count: usize,
    pub results: Vec<SearchHit>,
    pub provider: &'static str,
    pub fallback_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchEndpoints {
    pub duckduckgo: Url,
    pub bing: Url,
}

impl Default for SearchEndpoints {
    fn default() -> Self {
        Self {
            duckduckgo: Url::parse("https://html.duckduckgo.com/html/")
                .expect("static DuckDuckGo URL is valid"),
            bing: Url::parse("https://www.bing.com/search").expect("static Bing URL is valid"),
        }
    }
}

pub async fn read(context: &AppContext, url: &str) -> Result<ReadResponse> {
    read_with_max_chars(context, url, context.fetch.limits().max_output_chars).await
}

/// Read a page through the supplied shared context while applying a caller-specific
/// Unicode-scalar Markdown limit.
///
/// Presentation adapters use this narrow seam when their output contract is
/// stricter than the context-wide default; transport and network policy remain
/// owned by [`AppContext`].
pub async fn read_with_max_chars(
    context: &AppContext,
    url: &str,
    max_chars: usize,
) -> Result<ReadResponse> {
    let request = FetchRequest::auto(url)?;
    let page = context.fetch.fetch_request(request).await?;
    ensure_success(page.status, &page.url, None)?;
    let markdown = html_to_markdown_with_options(
        &page.html,
        &MarkdownOptions {
            base_url: Some(page.url.clone()),
            max_chars,
        },
    )?;
    Ok(ReadResponse {
        url: page.url,
        status: page.status,
        via: fetch_via_name(page.via),
        markdown,
    })
}

pub async fn search(
    context: &AppContext,
    query: &str,
    count: usize,
    scrape: bool,
) -> Result<SearchResponse> {
    search_with_endpoints(context, query, count, scrape, &SearchEndpoints::default()).await
}

pub async fn search_with_endpoints(
    context: &AppContext,
    query: &str,
    count: usize,
    scrape: bool,
    endpoints: &SearchEndpoints,
) -> Result<SearchResponse> {
    validate_search_input(query, count)?;
    let query = query.trim();
    let primary_url = search_url(&endpoints.duckduckgo, query, None);
    let primary = fetch_search_html(context, primary_url)
        .await
        .and_then(|html| parse_duckduckgo_results(&html, count));

    let (provider, hits, fallback_warning) = match primary {
        Ok(hits) if !hits.is_empty() => ("duckduckgo", hits, None),
        Ok(_) => {
            let warning =
                Some("DuckDuckGo returned a confirmed empty result set; used Bing fallback".into());
            let url = search_url(&endpoints.bing, query, Some(count));
            let html = fetch_search_html(context, url).await?;
            ("bing", parse_bing_results(&html, count)?, warning)
        }
        Err(error) => {
            let warning = Some(format!(
                "DuckDuckGo primary failed ({}); used Bing fallback",
                sanitized_error(&error)
            ));
            let url = search_url(&endpoints.bing, query, Some(count));
            let html = fetch_search_html(context, url).await?;
            ("bing", parse_bing_results(&html, count)?, warning)
        }
    };

    let results = if scrape {
        scrape_hits(context, hits).await
    } else {
        hits
    };
    Ok(SearchResponse {
        query: query.to_string(),
        count: results.len(),
        results,
        provider,
        fallback_warning,
    })
}

pub fn parse_duckduckgo_results(html: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let document = Html::parse_document(html);
    let container = selector("div.result", "duckduckgo")?;
    let link = selector("a.result__a", "duckduckgo")?;
    let snippet = selector(".result__snippet", "duckduckgo")?;
    let mut hits = Vec::new();

    for result in document.select(&container) {
        if hits.len() == limit.min(MAX_SEARCH_RESULTS) {
            break;
        }
        let Some(anchor) = result.select(&link).next() else {
            continue;
        };
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Some(url) = decode_duckduckgo_url(href) else {
            continue;
        };
        let title = bounded_text(anchor, MAX_TITLE_CHARS);
        if title.is_empty() {
            continue;
        }
        let snippet = result
            .select(&snippet)
            .next()
            .map(|element| bounded_text(element, MAX_SNIPPET_CHARS))
            .unwrap_or_default();
        hits.push(SearchHit {
            title,
            url,
            snippet,
            markdown: None,
            scrape_error: None,
        });
    }

    if hits.is_empty() && !confirmed_empty(html) {
        return Err(Error::UpstreamLayout {
            service: "duckduckgo",
        });
    }
    Ok(hits)
}

pub fn parse_bing_results(html: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let document = Html::parse_document(html);
    let container = selector("li.b_algo", "bing")?;
    let link = selector("h2 a", "bing")?;
    let snippet = selector(".b_caption p", "bing")?;
    let mut hits = Vec::new();

    for result in document.select(&container) {
        if hits.len() == limit.min(MAX_SEARCH_RESULTS) {
            break;
        }
        let Some(anchor) = result.select(&link).next() else {
            continue;
        };
        let Some(url) = anchor
            .value()
            .attr("href")
            .and_then(|href| Url::parse(href).ok())
            .filter(http_url)
        else {
            continue;
        };
        let title = bounded_text(anchor, MAX_TITLE_CHARS);
        if title.is_empty() {
            continue;
        }
        let snippet = result
            .select(&snippet)
            .next()
            .map(|element| bounded_text(element, MAX_SNIPPET_CHARS))
            .unwrap_or_default();
        hits.push(SearchHit {
            title,
            url,
            snippet,
            markdown: None,
            scrape_error: None,
        });
    }

    if hits.is_empty() && !confirmed_empty(html) {
        return Err(Error::UpstreamLayout { service: "bing" });
    }
    Ok(hits)
}

async fn fetch_search_html(context: &AppContext, url: Url) -> Result<String> {
    let page = context
        .fetch
        .fetch_request(FetchRequest::request(url.as_str())?)
        .await?;
    ensure_success(page.status, &page.url, None)?;
    Ok(page.html)
}

async fn scrape_hits(context: &AppContext, hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let hit_count = hits.len();
    let total_budget = context.fetch.limits().max_output_chars;
    let base_budget = total_budget.checked_div(hit_count).unwrap_or_default();
    let remainder = total_budget.checked_rem(hit_count).unwrap_or_default();
    let mut completed = stream::iter(hits.into_iter().enumerate())
        .map(|(index, mut hit)| async move {
            let max_chars = base_budget + usize::from(index < remainder);
            match read_with_max_chars(context, hit.url.as_str(), max_chars).await {
                Ok(response) => hit.markdown = Some(response.markdown),
                Err(error) => hit.scrape_error = Some(sanitized_error(&error)),
            }
            (index, hit)
        })
        .buffer_unordered(SCRAPE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    completed.sort_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, hit)| hit).collect()
}

pub fn validate_search_input(query: &str, count: usize) -> Result<()> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::InvalidInput("search query cannot be empty".into()));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(Error::InvalidInput(format!(
            "search query cannot exceed {MAX_QUERY_CHARS} characters"
        )));
    }
    if !(1..=MAX_SEARCH_RESULTS).contains(&count) {
        return Err(Error::InvalidInput(format!(
            "search result count must be between 1 and {MAX_SEARCH_RESULTS}"
        )));
    }
    Ok(())
}

fn search_url(base: &Url, query: &str, count: Option<usize>) -> Url {
    let mut url = base.clone();
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query);
        if let Some(count) = count {
            pairs.append_pair("count", &count.to_string());
        }
    }
    url
}

fn decode_duckduckgo_url(href: &str) -> Option<Url> {
    let base = Url::parse("https://duckduckgo.com/").ok()?;
    let wrapped = base.join(href).ok()?;
    if wrapped
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("duckduckgo.com"))
    {
        if let Some((_, target)) = wrapped.query_pairs().find(|(name, _)| name == "uddg") {
            return Url::parse(&target).ok().filter(http_url);
        }
    }
    http_url(&wrapped).then_some(wrapped)
}

fn selector(css: &str, service: &'static str) -> Result<Selector> {
    Selector::parse(css).map_err(|_| Error::Parse {
        kind: service,
        message: "internal result selector is invalid".into(),
    })
}

fn bounded_text(element: ElementRef<'_>, max_chars: usize) -> String {
    let text = element.text().collect::<String>();
    truncate_chars(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        max_chars,
    )
}

fn confirmed_empty(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.contains("no results found")
        || lower.contains("did not match any documents")
        || lower.contains("class=\"no-results\"")
        || lower.contains("class='no-results'")
}

fn http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

fn fetch_via_name(via: FetchVia) -> &'static str {
    match via {
        FetchVia::Request => "request",
        FetchVia::Browser => "browser",
        FetchVia::Test => "test",
    }
}

pub(crate) fn ensure_success(status: u16, url: &Url, retry_after_secs: Option<u64>) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    if status == 429 {
        return Err(Error::RateLimited { retry_after_secs });
    }
    Err(Error::HttpStatus {
        status,
        url: url.clone(),
    })
}

pub(crate) fn sanitized_error(error: &Error) -> String {
    match error {
        Error::InvalidInput(_) => "invalid input".into(),
        Error::Policy(_) => "network policy rejected the request".into(),
        Error::Dns(_) => "DNS resolution failed".into(),
        Error::Timeout { .. } => "request timed out".into(),
        Error::BodyLimit { .. } => "response exceeded the configured body limit".into(),
        Error::HttpStatus { status, .. } => format!("HTTP status {status}"),
        Error::Browser(_) => "browser rendering failed".into(),
        Error::Parse { kind, .. } => format!("{kind} response could not be parsed"),
        Error::Authentication(_) => "authentication is required or expired".into(),
        Error::RateLimited { retry_after_secs } => retry_after_secs.map_or_else(
            || "upstream rate limit reached".into(),
            |seconds| format!("upstream rate limit reached; retry after {seconds} seconds"),
        ),
        Error::RobotsDenied(_) => "robots policy denied the request".into(),
        Error::Cancelled => "operation was cancelled".into(),
        Error::UpstreamLayout { service } => format!("{service} response layout changed"),
        Error::Io(_) | Error::Http(_) => "transport failed".into(),
    }
}

/// Compatibility presentation shim used by the Task 6 feed/YouTube wrappers.
/// Task 8's typed services never call this function.
pub fn emit(json_out: bool, json_val: &serde_json::Value, text: &str) -> AnyResult<()> {
    crate::output::emit_json_value(json_out, json_val, text)
}
