//! Typed public and authenticated social-platform adapters.

use crate::context::AppContext;
use crate::cookies::{
    create_private_file_if_missing, ensure_private_directory, load_platform_cookies,
};
use crate::web::ensure_success;
use reqwest::cookie::CookieStore;
use reqwest::header::COOKIE;
use rscraper_core::{truncate_chars, Error, FetchRequest, Result};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

pub const MAX_SOCIAL_RESULTS: usize = 20;
const MAX_TEXT_CHARS: usize = 8_192;

#[derive(Debug, Clone, Serialize)]
pub struct RedditPost {
    pub title: String,
    pub url: Url,
    pub score: i64,
    pub author: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedditResponse {
    pub query: String,
    pub count: usize,
    pub results: Vec<RedditPost>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BilibiliVideo {
    pub title: String,
    pub url: Url,
    pub author: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BilibiliResponse {
    pub query: String,
    pub count: usize,
    pub results: Vec<BilibiliVideo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TwitterPost {
    pub author: String,
    pub text: String,
    pub url: Url,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TwitterResponse {
    pub platform: &'static str,
    pub query: Option<String>,
    pub count: usize,
    pub results: Vec<TwitterPost>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct XiaohongshuNote {
    pub id: String,
    pub title: String,
    pub author: String,
    pub url: Url,
}

#[derive(Debug, Clone, Serialize)]
pub struct XiaohongshuResponse {
    pub platform: &'static str,
    pub query: String,
    pub count: usize,
    pub results: Vec<XiaohongshuNote>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkedinPerson {
    pub name: String,
    pub headline: String,
    pub url: Url,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkedinResponse {
    pub platform: &'static str,
    pub query: String,
    pub count: usize,
    pub results: Vec<LinkedinPerson>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupResponse {
    pub platform: String,
    pub needs_login: &'static str,
    pub steps: Vec<String>,
    pub cookie_path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct RedditWire {
    data: Option<RedditListing>,
}

#[derive(Deserialize)]
struct RedditListing {
    children: Option<Vec<RedditChild>>,
}

#[derive(Deserialize)]
struct RedditChild {
    data: Option<RedditPostWire>,
}

#[derive(Deserialize)]
struct RedditPostWire {
    #[serde(default)]
    title: String,
    #[serde(default)]
    permalink: String,
    #[serde(default)]
    score: i64,
    #[serde(default)]
    author: String,
    #[serde(default)]
    selftext: String,
}

#[derive(Deserialize)]
struct BilibiliWire {
    code: i64,
    data: Option<BilibiliData>,
}

#[derive(Deserialize)]
struct BilibiliData {
    result: Option<Vec<BilibiliItemWire>>,
}

#[derive(Deserialize)]
struct BilibiliItemWire {
    #[serde(default)]
    title: String,
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct XiaohongshuState {
    search: XiaohongshuSearch,
}

#[derive(Deserialize)]
struct XiaohongshuSearch {
    #[serde(default)]
    notes: Vec<XiaohongshuNoteWire>,
}

#[derive(Deserialize)]
struct XiaohongshuNoteWire {
    id: String,
    title: String,
    #[serde(default)]
    author: String,
    url: String,
}

pub fn parse_reddit_response(bytes: &[u8], query: &str, limit: usize) -> Result<RedditResponse> {
    validate_query_count(query, limit)?;
    let wire: RedditWire =
        serde_json::from_slice(bytes).map_err(|error| json_error("reddit", error))?;
    let children = wire
        .data
        .and_then(|data| data.children)
        .ok_or(Error::UpstreamLayout { service: "reddit" })?;
    let raw_count = children.len();
    let base = Url::parse("https://www.reddit.com/").expect("static Reddit URL is valid");
    let results = children
        .into_iter()
        .filter_map(|child| {
            let item = child.data?;
            if item.title.trim().is_empty() || item.permalink.trim().is_empty() {
                return None;
            }
            let url = base
                .join(item.permalink.trim())
                .ok()
                .filter(|url| platform_url(url, &["reddit.com"]))?;
            Some(RedditPost {
                title: bounded(&item.title),
                url,
                score: item.score,
                author: bounded(&item.author),
                text: bounded(&item.selftext),
            })
        })
        .take(limit.min(MAX_SOCIAL_RESULTS))
        .collect::<Vec<_>>();
    if raw_count != 0 && results.is_empty() {
        return Err(Error::UpstreamLayout { service: "reddit" });
    }
    Ok(RedditResponse {
        query: query.trim().to_string(),
        count: results.len(),
        results,
    })
}

pub fn parse_bilibili_response(
    bytes: &[u8],
    query: &str,
    limit: usize,
) -> Result<BilibiliResponse> {
    validate_query_count(query, limit)?;
    let wire: BilibiliWire =
        serde_json::from_slice(bytes).map_err(|error| json_error("bilibili", error))?;
    if wire.code != 0 {
        return Err(Error::Parse {
            kind: "bilibili",
            message: "upstream returned a non-success result code".into(),
        });
    }
    let items = wire
        .data
        .and_then(|data| data.result)
        .ok_or(Error::UpstreamLayout {
            service: "bilibili",
        })?;
    let raw_count = items.len();
    let results = items
        .into_iter()
        .filter_map(|item| {
            let bvid = canonical_bvid(&item.bvid)?;
            let title = bounded(&strip_keyword_markup(&item.title));
            if title.is_empty() {
                return None;
            }
            let mut url = Url::parse("https://www.bilibili.com/video/")
                .expect("static Bilibili video URL is valid");
            url.path_segments_mut().ok()?.pop_if_empty().push(bvid);
            Some(BilibiliVideo {
                title,
                url,
                author: bounded(&item.author),
                description: bounded(&item.description),
            })
        })
        .take(limit.min(MAX_SOCIAL_RESULTS))
        .collect::<Vec<_>>();
    if raw_count != 0 && results.is_empty() {
        return Err(Error::UpstreamLayout {
            service: "bilibili",
        });
    }
    Ok(BilibiliResponse {
        query: query.trim().to_string(),
        count: results.len(),
        results,
    })
}

pub fn parse_twitter_response(html: &str, query: Option<&str>) -> Result<TwitterResponse> {
    reject_auth_page("twitter", html)?;
    let document = Html::parse_document(html);
    let article_selector = selector("article", "twitter")?;
    let text_selector = selector("[data-testid='tweetText']", "twitter")?;
    let author_selector = selector("[data-testid='User-Name']", "twitter")?;
    let status_selector = selector("a[href*='/status/']", "twitter")?;
    let time_selector = selector("time", "twitter")?;
    let base = Url::parse("https://x.com/").expect("static X URL is valid");
    let mut results = Vec::new();

    for article in document.select(&article_selector).take(MAX_SOCIAL_RESULTS) {
        let Some(text) = article.select(&text_selector).next().map(text_of) else {
            continue;
        };
        let Some(url) = article
            .select(&status_selector)
            .find_map(|anchor| anchor.value().attr("href"))
            .and_then(|href| base.join(href).ok())
            .filter(|url| platform_url(url, &["x.com", "twitter.com"]))
        else {
            continue;
        };
        let author = article
            .select(&author_selector)
            .next()
            .map(text_of)
            .unwrap_or_default();
        let published_at = article
            .select(&time_selector)
            .next()
            .and_then(|time| time.value().attr("datetime"))
            .map(str::to_string);
        results.push(TwitterPost {
            author,
            text,
            url,
            published_at,
        });
    }
    if results.is_empty() {
        return Err(Error::UpstreamLayout { service: "twitter" });
    }
    let content = results
        .iter()
        .map(|post| format!("{}\n{}\n{}", post.author, post.text, post.url))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(TwitterResponse {
        platform: "twitter",
        query: query.map(str::to_string),
        count: results.len(),
        results,
        content,
    })
}

pub fn parse_xiaohongshu_response(html: &str, query: &str) -> Result<XiaohongshuResponse> {
    reject_auth_page("xiaohongshu", html)?;
    let document = Html::parse_document(html);
    let script_selector = selector("script#__INITIAL_STATE__", "xiaohongshu")?;
    let script = document
        .select(&script_selector)
        .next()
        .map(|script| script.text().collect::<String>())
        .ok_or(Error::UpstreamLayout {
            service: "xiaohongshu",
        })?;
    let state: XiaohongshuState =
        serde_json::from_str(&script).map_err(|error| json_error("xiaohongshu", error))?;
    let base = Url::parse("https://www.xiaohongshu.com/").expect("static Xiaohongshu URL is valid");
    let results = state
        .search
        .notes
        .into_iter()
        .filter_map(|note| {
            let url = base
                .join(&note.url)
                .ok()
                .filter(|url| platform_url(url, &["xiaohongshu.com"]))?;
            Some(XiaohongshuNote {
                id: bounded(&note.id),
                title: bounded(&note.title),
                author: bounded(&note.author),
                url,
            })
        })
        .take(MAX_SOCIAL_RESULTS)
        .collect::<Vec<_>>();
    if results.is_empty() {
        return Err(Error::UpstreamLayout {
            service: "xiaohongshu",
        });
    }
    let content = results
        .iter()
        .map(|note| format!("{} — {}\n{}", note.title, note.author, note.url))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(XiaohongshuResponse {
        platform: "xiaohongshu",
        query: query.to_string(),
        count: results.len(),
        results,
        content,
    })
}

pub fn parse_linkedin_response(html: &str, query: &str) -> Result<LinkedinResponse> {
    reject_auth_page("linkedin", html)?;
    let document = Html::parse_document(html);
    let result_selector = selector(".reusable-search__result-container", "linkedin")?;
    let title_selector = selector(".entity-result__title-text a", "linkedin")?;
    let headline_selector = selector(".entity-result__primary-subtitle", "linkedin")?;
    let base = Url::parse("https://www.linkedin.com/").expect("static LinkedIn URL is valid");
    let mut results = Vec::new();
    for result in document.select(&result_selector).take(MAX_SOCIAL_RESULTS) {
        let Some(anchor) = result.select(&title_selector).next() else {
            continue;
        };
        let Some(url) = anchor
            .value()
            .attr("href")
            .and_then(|href| base.join(href).ok())
            .filter(|url| platform_url(url, &["linkedin.com"]))
        else {
            continue;
        };
        let headline = result
            .select(&headline_selector)
            .next()
            .map(text_of)
            .unwrap_or_default();
        results.push(LinkedinPerson {
            name: text_of(anchor),
            headline,
            url,
        });
    }
    if results.is_empty() {
        return Err(Error::UpstreamLayout {
            service: "linkedin",
        });
    }
    let content = results
        .iter()
        .map(|person| format!("{} — {}\n{}", person.name, person.headline, person.url))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(LinkedinResponse {
        platform: "linkedin",
        query: query.to_string(),
        count: results.len(),
        results,
        content,
    })
}

pub async fn reddit(context: &AppContext, query: &str, count: usize) -> Result<RedditResponse> {
    validate_query_count(query, count)?;
    let mut url = Url::parse("https://www.reddit.com/search.json")
        .expect("static Reddit search URL is valid");
    url.query_pairs_mut()
        .append_pair("q", query.trim())
        .append_pair("limit", &count.to_string());
    let mut request = FetchRequest::request(url.as_str())?;
    let cookie_path = context.config_dir.join("reddit.cookies.txt");
    if cookie_path.exists() {
        attach_cookie(&mut request, &cookie_path, &url)?;
    }
    let page = context.fetch.fetch_request(request).await?;
    ensure_success(page.status, &page.url, None)?;
    parse_reddit_response(page.html.as_bytes(), query, count)
}

pub async fn twitter(context: &AppContext, query: Option<&str>) -> Result<TwitterResponse> {
    let mut url = if query.is_some() {
        Url::parse("https://x.com/search").expect("static X search URL is valid")
    } else {
        Url::parse("https://x.com/home").expect("static X home URL is valid")
    };
    if let Some(query) = query {
        if query.trim().is_empty() {
            return Err(Error::InvalidInput("query cannot be empty".into()));
        }
        url.query_pairs_mut().append_pair("q", query.trim());
    }
    let mut request = FetchRequest::request(url.as_str())?;
    attach_cookie(
        &mut request,
        &context.config_dir.join("twitter.cookies.txt"),
        &Url::parse("https://x.com/").expect("static X origin is valid"),
    )?;
    let page = context.fetch.fetch_request(request).await?;
    ensure_authenticated_success("twitter", page.status, &page.url)?;
    parse_twitter_response(&page.html, query)
}

pub async fn xiaohongshu(context: &AppContext, query: &str) -> Result<XiaohongshuResponse> {
    if query.trim().is_empty() {
        return Err(Error::InvalidInput("query cannot be empty".into()));
    }
    let mut url = Url::parse("https://www.xiaohongshu.com/search_result")
        .expect("static Xiaohongshu search URL is valid");
    url.query_pairs_mut().append_pair("keyword", query.trim());
    let mut request = FetchRequest::request(url.as_str())?;
    attach_cookie(
        &mut request,
        &context.config_dir.join("xiaohongshu.cookies.txt"),
        &Url::parse("https://www.xiaohongshu.com/").expect("static Xiaohongshu origin is valid"),
    )?;
    let page = context.fetch.fetch_request(request).await?;
    ensure_authenticated_success("xiaohongshu", page.status, &page.url)?;
    parse_xiaohongshu_response(&page.html, query.trim())
}

pub async fn linkedin(context: &AppContext, query: &str) -> Result<LinkedinResponse> {
    if query.trim().is_empty() {
        return Err(Error::InvalidInput("query cannot be empty".into()));
    }
    let mut url = Url::parse("https://www.linkedin.com/search/results/people/")
        .expect("static LinkedIn search URL is valid");
    url.query_pairs_mut().append_pair("keywords", query.trim());
    let mut request = FetchRequest::request(url.as_str())?;
    attach_cookie(
        &mut request,
        &context.config_dir.join("linkedin.cookies.txt"),
        &Url::parse("https://www.linkedin.com/").expect("static LinkedIn origin is valid"),
    )?;
    let page = context.fetch.fetch_request(request).await?;
    ensure_authenticated_success("linkedin", page.status, &page.url)?;
    parse_linkedin_response(&page.html, query.trim())
}

pub fn setup(context: &AppContext, platform: &str) -> Result<SetupResponse> {
    let requested = platform.trim();
    if requested.is_empty()
        || requested.chars().count() > 64
        || requested.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput("platform name is invalid".into()));
    }
    let normalized = requested.to_ascii_lowercase();
    let canonical = match normalized.as_str() {
        "twitter" | "x" => "twitter",
        "reddit" => "reddit",
        "bilibili" => "bilibili",
        "xiaohongshu" | "xhs" => "xiaohongshu",
        "linkedin" => "linkedin",
        _ => {
            return Err(Error::InvalidInput(format!(
                "unknown platform `{requested}`; supported platforms: twitter, reddit, bilibili, xiaohongshu, linkedin"
            )))
        }
    };

    let (needs_login, steps, file_name, template): (
        &'static str,
        Vec<String>,
        Option<&'static str>,
        Option<&'static [u8]>,
    ) = match canonical {
        "twitter" => (
            "yes",
            vec![
                "Log in to X/Twitter in your normal browser.".into(),
                "Export only the cookies needed for x.com.".into(),
                "Save them as name=value lines in the private file shown below.".into(),
            ],
            Some("twitter.cookies.txt"),
            Some(b"# Replace placeholders, then keep this file mode 0600\nauth_token=<paste>\nct0=<paste>\n"),
        ),
        "reddit" => (
            "optional",
            vec![
                "Public search works without login.".into(),
                "If needed, export a reddit_session cookie into the private file.".into(),
            ],
            Some("reddit.cookies.txt"),
            Some(b"# Optional; replace the placeholder\nreddit_session=<paste>\n"),
        ),
        "bilibili" => (
            "no",
            vec!["Bilibili search uses the public API; no cookie file is created.".into()],
            None,
            None,
        ),
        "xiaohongshu" => (
            "yes",
            vec![
                "Log in to Xiaohongshu in your normal browser.".into(),
                "Export the web_session cookie into the private file.".into(),
            ],
            Some("xiaohongshu.cookies.txt"),
            Some(b"# Replace the placeholder\nweb_session=<paste>\n"),
        ),
        "linkedin" => (
            "yes",
            vec![
                "Log in to LinkedIn in your normal browser.".into(),
                "Export li_at and JSESSIONID into the private file.".into(),
            ],
            Some("linkedin.cookies.txt"),
            Some(b"# Replace placeholders\nli_at=<paste>\nJSESSIONID=<paste>\n"),
        ),
        _ => unreachable!("canonical platform is exhaustive"),
    };

    let cookie_path = if let (Some(file_name), Some(template)) = (file_name, template) {
        ensure_private_directory(&context.config_dir)?;
        let path = context.config_dir.join(file_name);
        create_private_file_if_missing(&path, template)?;
        Some(path)
    } else {
        None
    };

    Ok(SetupResponse {
        platform: canonical.to_string(),
        needs_login,
        steps,
        cookie_path,
    })
}

pub async fn bilibili(context: &AppContext, query: &str, count: usize) -> Result<BilibiliResponse> {
    validate_query_count(query, count)?;
    let mut url = Url::parse("https://api.bilibili.com/x/web-interface/search/type")
        .expect("static Bilibili search URL is valid");
    url.query_pairs_mut()
        .append_pair("search_type", "video")
        .append_pair("keyword", query.trim());
    let page = context
        .fetch
        .fetch_request(FetchRequest::request(url.as_str())?)
        .await?;
    ensure_success(page.status, &page.url, None)?;
    parse_bilibili_response(page.html.as_bytes(), query, count)
}

fn reject_auth_page(service: &'static str, html: &str) -> Result<()> {
    let lower = html.to_ascii_lowercase();
    let authentication_marker = match service {
        "twitter" => {
            lower.contains("/i/flow/login")
                || lower.contains("log in to x")
                || lower.contains("login to twitter")
                || lower.contains("consent.twitter.com")
        }
        "xiaohongshu" => {
            lower.contains("login-container")
                || lower.contains("passport.xiaohongshu.com")
                || lower.contains("请登录")
        }
        "linkedin" => {
            lower.contains("/checkpoint/")
                || lower.contains("security verification")
                || lower.contains("/uas/login")
                || lower.contains("authwall")
        }
        _ => false,
    };
    if authentication_marker {
        return Err(Error::Authentication(format!(
            "{service} session is missing, expired, or requires verification"
        )));
    }
    Ok(())
}

fn attach_cookie(request: &mut FetchRequest, path: &std::path::Path, origin: &Url) -> Result<()> {
    if matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ) {
        return Err(Error::Authentication(
            "platform cookies are not configured; run the matching setup command".into(),
        ));
    }
    let jar = load_platform_cookies(path, origin)?;
    let mut header = jar.cookies(&request.url).ok_or_else(|| {
        Error::Authentication("cookie file has no cookies scoped to this platform request".into())
    })?;
    header.set_sensitive(true);
    request.headers.insert(COOKIE, header);
    Ok(())
}

fn ensure_authenticated_success(service: &'static str, status: u16, url: &Url) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    if matches!(status, 401 | 403) {
        return Err(Error::Authentication(format!(
            "{service} session is missing, expired, or requires verification"
        )));
    }
    ensure_success(status, url, None)
}

fn validate_query_count(query: &str, count: usize) -> Result<()> {
    if query.trim().is_empty() {
        return Err(Error::InvalidInput("query cannot be empty".into()));
    }
    if !(1..=MAX_SOCIAL_RESULTS).contains(&count) {
        return Err(Error::InvalidInput(format!(
            "result count must be between 1 and {MAX_SOCIAL_RESULTS}"
        )));
    }
    Ok(())
}

fn selector(css: &str, service: &'static str) -> Result<Selector> {
    Selector::parse(css).map_err(|_| Error::Parse {
        kind: service,
        message: "internal result selector is invalid".into(),
    })
}

fn text_of(element: ElementRef<'_>) -> String {
    bounded(&element.text().collect::<Vec<_>>().join(" "))
}

fn bounded(value: &str) -> String {
    truncate_chars(
        &value.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_TEXT_CHARS,
    )
}

fn strip_keyword_markup(value: &str) -> String {
    Html::parse_fragment(value)
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
}

fn canonical_bvid(value: &str) -> Option<&str> {
    (value.len() == 12
        && value.starts_with("BV")
        && value[2..].bytes().all(|byte| byte.is_ascii_alphanumeric()))
    .then_some(value)
}

fn http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

fn platform_url(url: &Url, allowed_suffixes: &[&str]) -> bool {
    if !http_url(url) {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allowed_suffixes
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

fn json_error(kind: &'static str, error: serde_json::Error) -> Error {
    Error::Parse {
        kind,
        message: format!(
            "invalid JSON at line {} column {}",
            error.line(),
            error.column()
        ),
    }
}
