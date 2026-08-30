//! Tor-only search and bounded source retrieval.

use crate::{
    normalize_query, validate_single_line, validate_url_bound, validate_url_text,
    validate_v3_onion_url, Error, ErrorCode, Hit, Result, MAX_HITS,
};
use async_trait::async_trait;
use rscraper_core::markdown::{html_to_markdown_with_options, MarkdownOptions};
use rscraper_core::{
    looks_like_javascript_shell, BrowserEgress, BrowserRenderer, FetchClient, FetchHostRestriction,
    FetchMode, FetchRequest, FetchStep, NetworkPolicy, OperationLimits, Page,
};
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use url::{Host, Url};

pub const DEFAULT_TOR_PROXY: &str = "socks5h://127.0.0.1:9050/";
const TOR_CHECK_URL: &str = "https://check.torproject.org/api/ip";
const MAX_SOURCE_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOURCE_CHARS: usize = 40_000;
const MAX_TITLE_CHARS: usize = 512;
const MAX_SNIPPET_CHARS: usize = 4_096;
const MAX_ENGINE_NAME_CHARS: usize = 32;
const MAX_ENGINES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResearchPurpose {
    Search,
    Source,
    Browser,
}

#[derive(Clone)]
pub struct ResearchRequest {
    pub url: Url,
    pub proxy: Url,
    pub purpose: ResearchPurpose,
}

impl fmt::Debug for ResearchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResearchRequest")
            .field("url", &"<redacted>")
            .field("proxy", &"<redacted>")
            .field("purpose", &self.purpose)
            .finish()
    }
}

#[async_trait]
pub trait ResearchTransport: Send + Sync {
    fn proxy(&self) -> &Url;
    async fn fetch(&self, request: ResearchRequest) -> Result<Page>;
}

#[derive(Clone)]
pub struct SearchEngine {
    name: String,
    endpoint: Url,
}

impl SearchEngine {
    pub fn new(name: &str, endpoint: Url) -> Result<Self> {
        let name = validate_single_line("search engine name", name, 1, MAX_ENGINE_NAME_CHARS)?;
        validate_research_url(&endpoint, false)?;
        Ok(Self { name, endpoint })
    }
}

impl fmt::Debug for SearchEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchEngine")
            .field("name", &self.name)
            .field("endpoint", &"<redacted>")
            .finish()
    }
}

pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    pub warnings: Vec<String>,
}

impl fmt::Debug for SearchOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchOutcome")
            .field("hit_count", &self.hits.len())
            .field("warning_count", &self.warnings.len())
            .finish()
    }
}

pub struct TorTransport {
    pub proxy: Url,
    fetch: FetchClient,
    browser: Arc<dyn TorBrowser>,
}

impl fmt::Debug for TorTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TorTransport")
            .field("proxy", &"<redacted>")
            .field("fetch_policy", &self.fetch.policy())
            .field("limits", self.fetch.limits())
            .finish()
    }
}

impl TorTransport {
    pub async fn connect(proxy: Url, limits: OperationLimits) -> Result<Self> {
        Self::connect_with_probe(proxy, limits, &CoreTorProbe).await
    }

    async fn connect_with_probe(
        proxy: Url,
        limits: OperationLimits,
        probe: &dyn TorProbe,
    ) -> Result<Self> {
        let proxy = parse_tor_proxy(proxy.as_str())?;
        let limits = source_limits(limits)?;
        let fetch = FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .limits(limits)
            .build()
            .map_err(|error| Error::from_core(error, "Tor transport"))?;
        probe.check(&fetch, &proxy).await?;
        Ok(Self {
            proxy,
            fetch,
            browser: Arc::new(CoreTorBrowser),
        })
    }

    pub async fn fetch_html(&self, url: Url) -> Result<Page> {
        self.fetch_document(url, true).await
    }

    async fn fetch_document(&self, url: Url, onion_required: bool) -> Result<Page> {
        validate_research_url(&url, onion_required)?;
        let deadline = self.fetch.limits().request_timeout;
        tokio::time::timeout(deadline, self.fetch_document_inner(url, onion_required))
            .await
            .map_err(|_| Error::new(ErrorCode::Timeout, "Tor document request"))?
    }

    async fn fetch_document_inner(&self, url: Url, onion_required: bool) -> Result<Page> {
        let original_host = normalized_host(&url)?;
        let mut request = proxied_request(&url, &self.proxy, FetchMode::Request)?;
        let mut redirects = 0usize;
        let page = loop {
            match self
                .fetch
                .fetch_request_one_hop(request)
                .await
                .map_err(|error| Error::from_core(error, "Tor document request"))?
            {
                FetchStep::Redirect(redirect) => {
                    if redirects >= self.fetch.limits().max_redirects {
                        return Err(Error::new(ErrorCode::Policy, "Tor redirect limit"));
                    }
                    validate_research_url(&redirect.next_request.url, onion_required)?;
                    if normalized_host(&redirect.next_request.url)? != original_host {
                        return Err(Error::new(ErrorCode::Policy, "cross-host redirect"));
                    }
                    request = redirect.next_request;
                    request.proxy = Some(self.proxy.clone());
                    redirects += 1;
                }
                FetchStep::Response(page) => break page,
            }
        };

        validate_html_page(&page)?;
        if looks_like_javascript_shell(&page.html) {
            return self.render_shell(&page, &original_host).await;
        }
        Ok(page)
    }

    async fn render_shell(&self, page: &Page, original_host: &str) -> Result<Page> {
        let rendered = self
            .browser
            .render(&self.proxy, &page.url, self.fetch.limits())
            .await?;
        validate_html_page(&rendered)?;
        if normalized_host(&rendered.url)? != original_host {
            return Err(Error::new(ErrorCode::Policy, "cross-host browser result"));
        }
        Ok(rendered)
    }
}

#[async_trait]
trait TorProbe: Send + Sync {
    async fn check(&self, fetch: &FetchClient, proxy: &Url) -> Result<()>;
}

struct CoreTorProbe;

#[async_trait]
impl TorProbe for CoreTorProbe {
    async fn check(&self, fetch: &FetchClient, proxy: &Url) -> Result<()> {
        let url = Url::parse(TOR_CHECK_URL)
            .map_err(|_| Error::new(ErrorCode::Configuration, "Tor check endpoint"))?;
        let mut request = FetchRequest::request(url.as_str())
            .map_err(|error| Error::from_core(error, "Tor check request"))?;
        request.proxy = Some(proxy.clone());
        request.host_restriction = Some(
            FetchHostRestriction::https_label_suffixes(["check.torproject.org"])
                .map_err(|error| Error::from_core(error, "Tor check policy"))?,
        );
        let response = fetch
            .fetch_raw_request(request)
            .await
            .map_err(|_| Error::tor_unavailable())?;
        if !(200..300).contains(&response.status) || response.bytes.len() > 64 * 1024 {
            return Err(Error::tor_unavailable());
        }
        let check: TorCheck =
            serde_json::from_slice(&response.bytes).map_err(|_| Error::tor_unavailable())?;
        if !check.is_tor {
            return Err(Error::tor_unavailable());
        }
        Ok(())
    }
}

#[async_trait]
trait TorBrowser: Send + Sync {
    async fn render(&self, proxy: &Url, url: &Url, limits: &OperationLimits) -> Result<Page>;
}

struct CoreTorBrowser;

#[async_trait]
impl TorBrowser for CoreTorBrowser {
    async fn render(&self, proxy: &Url, url: &Url, limits: &OperationLimits) -> Result<Page> {
        let renderer = BrowserRenderer::discover(BrowserEgress::TorRequired {
            proxy: proxy.clone(),
        })
        .map_err(|_| Error::new(ErrorCode::Browser, "Tor browser discovery"))?;
        let browser_request = proxied_request(url, proxy, FetchMode::Browser)?;
        renderer
            .render(&browser_request, limits)
            .await
            .map_err(|_| Error::new(ErrorCode::Browser, "Tor browser render"))
    }
}

#[async_trait]
impl ResearchTransport for TorTransport {
    fn proxy(&self) -> &Url {
        &self.proxy
    }

    async fn fetch(&self, request: ResearchRequest) -> Result<Page> {
        if request.proxy != self.proxy {
            return Err(Error::new(ErrorCode::Policy, "research proxy identity"));
        }
        match request.purpose {
            ResearchPurpose::Search => self.fetch_document(request.url, false).await,
            ResearchPurpose::Source => self.fetch_document(request.url, true).await,
            ResearchPurpose::Browser => Err(Error::new(
                ErrorCode::InvalidInput,
                "direct browser request",
            )),
        }
    }
}

pub fn parse_tor_proxy(value: &str) -> Result<Url> {
    validate_url_text("Tor proxy", value)?;
    let proxy = Url::parse(value).map_err(|_| Error::new(ErrorCode::InvalidInput, "Tor proxy"))?;
    if proxy.scheme() != "socks5h"
        || !proxy.username().is_empty()
        || proxy.password().is_some()
        || proxy.path() != "/"
        || proxy.query().is_some()
        || proxy.fragment().is_some()
        || proxy.port().is_none()
        || proxy.port() == Some(0)
    {
        return Err(Error::new(ErrorCode::InvalidInput, "Tor proxy"));
    }
    let address = match proxy.host() {
        Some(Host::Ipv4(address)) => IpAddr::V4(address),
        Some(Host::Ipv6(address)) => IpAddr::V6(address),
        Some(Host::Domain(domain)) => domain
            .parse::<IpAddr>()
            .map_err(|_| Error::new(ErrorCode::InvalidInput, "Tor proxy"))?,
        None => return Err(Error::new(ErrorCode::InvalidInput, "Tor proxy")),
    };
    let unusable = match address {
        IpAddr::V4(address) => unusable_ipv4(address),
        IpAddr::V6(address) => {
            address.to_ipv4_mapped().is_some_and(unusable_ipv4)
                || address.is_unspecified()
                || address.is_multicast()
        }
    };
    if unusable {
        return Err(Error::new(ErrorCode::InvalidInput, "Tor proxy"));
    }
    Ok(proxy)
}

fn unusable_ipv4(address: Ipv4Addr) -> bool {
    address.is_unspecified() || address.is_multicast() || address == Ipv4Addr::BROADCAST
}

pub async fn search_with_transport(
    transport: &dyn ResearchTransport,
    engines: &[SearchEngine],
    query: &str,
) -> Result<SearchOutcome> {
    let query = normalize_query(query)?;
    if engines.is_empty() || engines.len() > MAX_ENGINES {
        return Err(Error::new(ErrorCode::InvalidInput, "search engines"));
    }
    let proxy = parse_tor_proxy(transport.proxy().as_str())?;
    let mut warnings = Vec::new();
    let mut confirmed_empty = false;
    for engine in engines {
        let mut url = engine.endpoint.clone();
        url.query_pairs_mut().append_pair("q", &query);
        validate_research_url(&url, false)?;
        let request = ResearchRequest {
            url,
            proxy: proxy.clone(),
            purpose: ResearchPurpose::Search,
        };
        match transport.fetch(request).await {
            Ok(page) => {
                if validate_html_page(&page).is_err() {
                    warnings.push("A search engine returned an unusable document".into());
                    continue;
                }
                match parse_search_results(&engine.name, &page.html) {
                    Ok(hits) if !hits.is_empty() => {
                        return Ok(SearchOutcome { hits, warnings });
                    }
                    Ok(_) => {
                        confirmed_empty = true;
                        warnings
                            .push("A search engine returned a confirmed empty result set".into());
                    }
                    Err(_) => warnings.push("A search engine layout could not be parsed".into()),
                }
            }
            Err(_) => warnings.push("A search engine request failed".into()),
        }
    }
    if confirmed_empty {
        Err(Error::new(ErrorCode::SearchEmpty, "search engines"))
    } else {
        Err(Error::new(ErrorCode::SearchLayout, "search engines"))
    }
}

pub fn parse_search_results(engine: &str, html: &str) -> Result<Vec<Hit>> {
    let document = Html::parse_document(html);
    let container = selector("article.result, li.result, div.result")?;
    let link = selector("a.result-link, a.result-title, h3 a, a[href]")?;
    let snippet = selector(".result-snippet, .snippet, p")?;
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for result in document.select(&container) {
        if hits.len() == MAX_HITS {
            break;
        }
        let Some(anchor) = result.select(&link).next() else {
            continue;
        };
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Ok(url) = search_result_url(engine, href) else {
            continue;
        };
        if !seen.insert(url.as_str().to_owned()) {
            continue;
        }
        let title = bounded_element_text(anchor, MAX_TITLE_CHARS);
        if title.is_empty() {
            continue;
        }
        let snippet = result
            .select(&snippet)
            .next()
            .map(|element| bounded_element_text(element, MAX_SNIPPET_CHARS))
            .unwrap_or_default();
        hits.push(Hit {
            title,
            url,
            snippet,
            source: None,
            source_warning: None,
        });
    }
    if hits.is_empty() && !confirmed_empty(html) {
        return Err(Error::new(ErrorCode::SearchLayout, "search result parser"));
    }
    Ok(hits)
}

fn search_result_url(engine: &str, href: &str) -> Result<Url> {
    if let Ok(url) = normalize_onion_url(href) {
        return Ok(url);
    }
    if !engine.eq_ignore_ascii_case("ahmia") {
        return Err(Error::new(ErrorCode::InvalidInput, "search result URL"));
    }

    validate_url_text("search result URL", href)?;
    validate_ahmia_wrapper_reference(href)?;
    let base = Url::parse("https://ahmia.fi/")
        .map_err(|_| Error::new(ErrorCode::Configuration, "Ahmia result wrapper"))?;
    let wrapper = base
        .join(href)
        .map_err(|_| Error::new(ErrorCode::InvalidInput, "search result URL"))?;
    if wrapper.scheme() != "https"
        || wrapper.host_str() != Some("ahmia.fi")
        || wrapper.port_or_known_default() != Some(443)
        || !wrapper.username().is_empty()
        || wrapper.password().is_some()
        || wrapper.path() != "/onion-redirect/"
        || wrapper.fragment().is_some()
    {
        return Err(Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"));
    }

    normalize_onion_url(&ahmia_redirect_destination(href)?)
}

fn validate_ahmia_wrapper_reference(href: &str) -> Result<()> {
    if href.starts_with('/') {
        if !href.starts_with("//") {
            return Ok(());
        }
        return Err(Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"));
    }

    let (scheme, remainder) = href
        .split_once("://")
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"))?;
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if scheme != "https" || !matches!(authority, "ahmia.fi" | "ahmia.fi:443") {
        return Err(Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"));
    }
    Ok(())
}

fn ahmia_redirect_destination(href: &str) -> Result<String> {
    let raw_query = href
        .split_once('?')
        .map(|(_, query)| query)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"))?;
    let mut parameters = raw_query.split('&');
    let parameter = parameters
        .next()
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"))?;
    if parameters.next().is_some() {
        return Err(Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"));
    }
    let (name, raw_value) = parameter
        .split_once('=')
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"))?;
    if name != "redirect_url" || raw_value.is_empty() {
        return Err(Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"));
    }
    let mut decoded = url::form_urlencoded::parse(parameter.as_bytes());
    let (decoded_name, value) = decoded
        .next()
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"))?;
    if decoded.next().is_some() || decoded_name != "redirect_url" {
        return Err(Error::new(ErrorCode::InvalidInput, "Ahmia result wrapper"));
    }
    Ok(value.into_owned())
}

pub async fn retrieve_sources(transport: &dyn ResearchTransport, hits: Vec<Hit>) -> Vec<Hit> {
    let proxy = match parse_tor_proxy(transport.proxy().as_str()) {
        Ok(proxy) => proxy,
        Err(_) => return hits.into_iter().take(MAX_HITS).collect(),
    };
    let mut output = Vec::new();
    for (index, mut hit) in hits.into_iter().take(MAX_HITS).enumerate() {
        if index < 5 {
            let request = ResearchRequest {
                url: hit.url.clone(),
                proxy: proxy.clone(),
                purpose: ResearchPurpose::Source,
            };
            match transport.fetch(request).await {
                Ok(page) if validate_html_page(&page).is_ok() => {
                    let markdown = html_to_markdown_with_options(
                        &page.html,
                        &MarkdownOptions {
                            base_url: Some(page.url.clone()),
                            max_chars: MAX_SOURCE_CHARS,
                        },
                    );
                    match markdown {
                        Ok(source) => hit.source = Some(source),
                        Err(_) => {
                            hit.source_warning =
                                Some("Source text exceeded the bounded conversion policy".into())
                        }
                    }
                }
                _ => {
                    hit.source_warning =
                        Some("Source retrieval failed through the Tor transport".into())
                }
            }
        }
        output.push(hit);
    }
    output
}

fn source_limits(mut limits: OperationLimits) -> Result<OperationLimits> {
    if limits.connect_timeout.is_zero()
        || limits.request_timeout.is_zero()
        || limits.max_redirects == 0
    {
        return Err(Error::new(ErrorCode::InvalidInput, "Tor limits"));
    }
    limits.max_body_bytes = limits.max_body_bytes.min(MAX_SOURCE_BODY_BYTES);
    if limits.max_body_bytes == 0 {
        return Err(Error::new(ErrorCode::InvalidInput, "Tor body limit"));
    }
    limits.max_output_chars = limits.max_output_chars.min(MAX_SOURCE_CHARS);
    if limits.max_output_chars == 0 {
        return Err(Error::new(ErrorCode::InvalidInput, "Tor output limit"));
    }
    Ok(limits)
}

fn proxied_request(url: &Url, proxy: &Url, mode: FetchMode) -> Result<FetchRequest> {
    let mut request = match mode {
        FetchMode::Request => FetchRequest::request(url.as_str()),
        FetchMode::Browser => FetchRequest::browser(url.as_str()),
        FetchMode::Auto => FetchRequest::auto(url.as_str()),
    }
    .map_err(|error| Error::from_core(error, "Tor request"))?;
    request.proxy = Some(proxy.clone());
    if mode == FetchMode::Browser {
        request.host_restriction = Some(
            FetchHostRestriction::http_or_https_exact_host(url)
                .map_err(|error| Error::from_core(error, "Tor browser host policy"))?,
        );
    }
    Ok(request)
}

fn validate_research_url(url: &Url, onion_required: bool) -> Result<()> {
    validate_url_bound("research URL", url)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::new(ErrorCode::InvalidInput, "research URL"));
    }
    if onion_required {
        validate_v3_onion_url(url)?;
    }
    Ok(())
}

fn normalize_onion_url(value: &str) -> Result<Url> {
    validate_url_text("search result URL", value)?;
    validate_canonical_onion_authority(value)?;
    let mut url =
        Url::parse(value).map_err(|_| Error::new(ErrorCode::InvalidInput, "search result URL"))?;
    url.set_fragment(None);
    validate_research_url(&url, true)?;
    Ok(url)
}

fn validate_canonical_onion_authority(value: &str) -> Result<()> {
    let (scheme, remainder) = value
        .split_once("://")
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "search result URL"))?;
    if !matches!(scheme, "http" | "https") {
        return Err(Error::new(ErrorCode::InvalidInput, "search result URL"));
    }
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(Error::new(ErrorCode::InvalidInput, "search result URL"));
    }
    let host = match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':')
                || port.is_empty()
                || !port.bytes().all(|byte| byte.is_ascii_digit())
                || port.parse::<u16>().ok().filter(|port| *port != 0).is_none()
            {
                return Err(Error::new(ErrorCode::InvalidInput, "search result URL"));
            }
            host
        }
        None => authority,
    };
    let Some(label) = host.strip_suffix(".onion") else {
        return Err(Error::new(ErrorCode::InvalidInput, "search result URL"));
    };
    if label.len() != 56
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'2'..=b'7'))
    {
        return Err(Error::new(ErrorCode::InvalidInput, "search result URL"));
    }
    Ok(())
}

fn normalized_host(url: &Url) -> Result<String> {
    url.host_str()
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "research host"))
}

fn validate_html_page(page: &Page) -> Result<()> {
    if !(200..300).contains(&page.status) {
        return Err(Error::new(ErrorCode::Upstream, "research HTTP status"));
    }
    if page.html.len() > MAX_SOURCE_BODY_BYTES {
        return Err(
            Error::new(ErrorCode::BodyLimit, "research HTML").with_limit(MAX_SOURCE_BODY_BYTES)
        );
    }
    let media_type = page
        .content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !media_type.is_some_and(|value| {
        value.eq_ignore_ascii_case("text/html")
            || value.eq_ignore_ascii_case("application/xhtml+xml")
    }) {
        return Err(Error::new(ErrorCode::Policy, "research content type"));
    }
    Ok(())
}

fn selector(value: &'static str) -> Result<Selector> {
    Selector::parse(value).map_err(|_| Error::new(ErrorCode::Configuration, "search selector"))
}

fn bounded_element_text(element: ElementRef<'_>, max_chars: usize) -> String {
    element
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn confirmed_empty(html: &str) -> bool {
    let document = Html::parse_document(html);
    Selector::parse(
        ".confirmed-empty, .no-results, #no-results, #noResults, [data-results-empty='true']",
    )
    .is_ok_and(|empty_state| document.select(&empty_state).next().is_some())
}

#[derive(Deserialize)]
struct TorCheck {
    #[serde(rename = "IsTor", alias = "is_tor")]
    is_tor: bool,
}

#[cfg(test)]
mod tests {
    use super::{
        parse_tor_proxy, proxied_request, TorBrowser, TorProbe, TorTransport, MAX_SOURCE_BODY_BYTES,
    };
    use crate::{Error, ErrorCode, Result};
    use async_trait::async_trait;
    use rscraper_core::{FetchClient, FetchMode, FetchVia, NetworkPolicy, OperationLimits, Page};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use url::Url;

    const VALID_ONION_A: &str = "aebagbafaydqqcikbmga2dqpcaireeyuculbogazdinryhi6d4qcmeqd.onion";
    const VALID_ONION_B: &str = "aibqibiga4eascqlbqgq4dyqcejbgfavcylrqgi2dmob2hq7eaqs4eqd.onion";

    #[test]
    fn browser_request_carries_the_exact_tor_proxy_and_same_host_restriction() {
        let proxy = parse_tor_proxy("socks5h://127.0.0.1:9050/").unwrap();
        let url = Url::parse(&format!("http://{VALID_ONION_A}/")).unwrap();
        let request = proxied_request(&url, &proxy, FetchMode::Browser).unwrap();
        assert_eq!(request.proxy.as_ref(), Some(&proxy));
        assert_eq!(request.mode, FetchMode::Browser);
        let restriction = request.host_restriction.as_ref().unwrap();
        restriction.validate(&url).unwrap();
        assert!(restriction
            .validate(&Url::parse(&format!("http://{VALID_ONION_B}/script.js")).unwrap())
            .is_err());
    }

    #[derive(Default)]
    struct RecordingProbe(Mutex<Vec<Url>>);

    #[async_trait]
    impl TorProbe for RecordingProbe {
        async fn check(&self, fetch: &FetchClient, proxy: &Url) -> Result<()> {
            assert_eq!(fetch.policy(), NetworkPolicy::AllowPrivate);
            assert!(fetch.limits().max_body_bytes <= MAX_SOURCE_BODY_BYTES);
            self.0.lock().unwrap().push(proxy.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn connection_cannot_skip_probe_and_uses_the_validated_proxy_identity() {
        let proxy = parse_tor_proxy("socks5h://127.0.0.1:9050/").unwrap();
        let probe = RecordingProbe::default();
        let transport =
            TorTransport::connect_with_probe(proxy.clone(), OperationLimits::default(), &probe)
                .await
                .unwrap();
        assert_eq!(
            probe.0.lock().unwrap().as_slice(),
            std::slice::from_ref(&proxy)
        );
        assert_eq!(transport.proxy, proxy);
        assert!(!format!("{transport:?}").contains("127.0.0.1"));
    }

    #[derive(Default)]
    struct FailingBrowser(Mutex<Vec<(Url, Url)>>);

    #[async_trait]
    impl TorBrowser for FailingBrowser {
        async fn render(&self, proxy: &Url, url: &Url, _limits: &OperationLimits) -> Result<Page> {
            self.0.lock().unwrap().push((proxy.clone(), url.clone()));
            Err(Error::new(ErrorCode::Browser, "recording browser"))
        }
    }

    #[tokio::test]
    async fn browser_failure_is_returned_after_one_same_proxy_call() {
        let proxy = parse_tor_proxy("socks5h://127.0.0.1:9050/").unwrap();
        let browser = Arc::new(FailingBrowser::default());
        let fetch = FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .build()
            .unwrap();
        let transport = TorTransport {
            proxy: proxy.clone(),
            fetch,
            browser: browser.clone(),
        };
        let url = Url::parse(&format!("http://{VALID_ONION_A}/")).unwrap();
        let shell = Page {
            url: url.clone(),
            status: 200,
            content_type: Some("text/html".into()),
            html: "<div id=app></div><script>boot()</script>".into(),
            via: FetchVia::Test,
        };
        let error = transport
            .render_shell(&shell, url.host_str().unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Browser);
        assert_eq!(browser.0.lock().unwrap().as_slice(), &[(proxy, url)]);
    }

    async fn socks_fixture(
        responses: Vec<Vec<u8>>,
    ) -> (Url, Arc<Mutex<Vec<(String, u16)>>>, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy = Url::parse(&format!("socks5h://{}/", listener.local_addr().unwrap())).unwrap();
        let destinations = Arc::new(Mutex::new(Vec::new()));
        let recorded = destinations.clone();
        let handle = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut greeting = [0_u8; 2];
                stream.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting[0], 5);
                let mut methods = vec![0_u8; usize::from(greeting[1])];
                stream.read_exact(&mut methods).await.unwrap();
                assert!(methods.contains(&0));
                stream.write_all(&[5, 0]).await.unwrap();

                let mut request = [0_u8; 4];
                stream.read_exact(&mut request).await.unwrap();
                assert_eq!(&request[..3], &[5, 1, 0]);
                let host = match request[3] {
                    1 => {
                        let mut address = [0_u8; 4];
                        stream.read_exact(&mut address).await.unwrap();
                        std::net::Ipv4Addr::from(address).to_string()
                    }
                    3 => {
                        let length = stream.read_u8().await.unwrap();
                        let mut domain = vec![0_u8; usize::from(length)];
                        stream.read_exact(&mut domain).await.unwrap();
                        String::from_utf8(domain).unwrap()
                    }
                    4 => {
                        let mut address = [0_u8; 16];
                        stream.read_exact(&mut address).await.unwrap();
                        std::net::Ipv6Addr::from(address).to_string()
                    }
                    other => panic!("unexpected SOCKS address type {other}"),
                };
                let port = stream.read_u16().await.unwrap();
                recorded.lock().unwrap().push((host, port));
                stream
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                    .await
                    .unwrap();

                let mut request_bytes = Vec::new();
                while !request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0);
                    request_bytes.extend_from_slice(&chunk[..read]);
                }
                stream.write_all(&response).await.unwrap();
            }
        });
        (proxy, destinations, handle)
    }

    fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut output = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            output.push_str(name);
            output.push_str(": ");
            output.push_str(value);
            output.push_str("\r\n");
        }
        output.push_str("\r\n");
        output
            .into_bytes()
            .into_iter()
            .chain(body.iter().copied())
            .collect()
    }

    fn fixture_transport(proxy: Url) -> TorTransport {
        let limits = OperationLimits {
            max_body_bytes: MAX_SOURCE_BODY_BYTES,
            ..OperationLimits::default()
        };
        let fetch = FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .limits(limits)
            .build()
            .unwrap();
        TorTransport {
            proxy,
            fetch,
            browser: Arc::new(super::CoreTorBrowser),
        }
    }

    #[tokio::test]
    async fn source_fetch_uses_remote_dns_and_same_proxy_for_same_host_redirects() {
        let html = b"<html><title>source</title><body><p>bounded source document</p></body></html>";
        let (proxy, destinations, proxy_task) = socks_fixture(vec![
            response(
                "302 Found",
                &[("Content-Type", "text/html"), ("Location", "/next")],
                b"",
            ),
            response("200 OK", &[("Content-Type", "text/html")], html),
        ])
        .await;
        let transport = fixture_transport(proxy.clone());
        let url = Url::parse(&format!("http://{VALID_ONION_A}/start")).unwrap();
        let page = transport.fetch_html(url).await.unwrap();
        assert_eq!(page.url.path(), "/next");
        proxy_task.await.unwrap();
        assert_eq!(
            destinations.lock().unwrap().as_slice(),
            &[(VALID_ONION_A.into(), 80,), (VALID_ONION_A.into(), 80,),]
        );
        assert_eq!(transport.proxy, proxy);
    }

    #[tokio::test]
    async fn source_fetch_rejects_cross_host_redirects_media_attachments_status_and_body_cap() {
        let onion = format!("http://{VALID_ONION_A}/");
        let cases = vec![
            (
                response(
                    "302 Found",
                    &[
                        ("Content-Type", "text/html"),
                        ("Location", &format!("http://{VALID_ONION_B}/")),
                    ],
                    b"",
                ),
                ErrorCode::Policy,
            ),
            (
                response("200 OK", &[("Content-Type", "image/png")], b"png"),
                ErrorCode::Policy,
            ),
            (
                response(
                    "200 OK",
                    &[
                        ("Content-Type", "text/html"),
                        ("Content-Disposition", "attachment; filename=x.html"),
                    ],
                    b"html",
                ),
                ErrorCode::Policy,
            ),
            (
                response("500 Error", &[("Content-Type", "text/html")], b"failure"),
                ErrorCode::Upstream,
            ),
        ];
        for (fixture_response, expected) in cases {
            let (proxy, destinations, proxy_task) = socks_fixture(vec![fixture_response]).await;
            let error = fixture_transport(proxy)
                .fetch_html(Url::parse(&onion).unwrap())
                .await
                .unwrap_err();
            assert_eq!(error.code(), expected);
            proxy_task.await.unwrap();
            assert_eq!(destinations.lock().unwrap().len(), 1);
        }

        let oversized_headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_SOURCE_BODY_BYTES + 1
        )
        .into_bytes();
        let (proxy, _, proxy_task) = socks_fixture(vec![oversized_headers]).await;
        let error = fixture_transport(proxy)
            .fetch_html(Url::parse(&onion).unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::BodyLimit);
        proxy_task.await.unwrap();
    }
}
