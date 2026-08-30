use crate::document::{FetchVia, Page};
use crate::limits::{MAX_CONNECT_TIMEOUT, MAX_REQUEST_TIMEOUT};
use crate::mime_policy::{
    reject_attachments, validate_content_type_declarations, ValidatedContentType,
};
use crate::policy::{
    address_is_allowed, map_transport_error, validate_url, NetworkPolicy, PolicyDnsError,
    ResolverSource, SafeResolver, SystemResolver,
};
use crate::{looks_like_javascript_shell, BrowserBackend};
use crate::{Error, OperationLimits, Result};
use encoding_rs::Encoding;
use futures_util::StreamExt;
use reqwest::header::{
    HeaderMap, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, LOCATION,
    PROXY_AUTHORIZATION, WWW_AUTHENTICATE,
};
use std::collections::VecDeque;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use tokio::time::Instant;
use url::{Host, Url};

/// Default user-agent sent by core-owned HTTP requests.
pub const DEFAULT_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const MAX_PROXY_CLIENTS: usize = 8;

/// Transport strategy for a [`FetchRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    /// Use the bounded HTTP transport.
    Request,
    /// Require an explicitly configured browser backend.
    Browser,
    /// Try HTTP and render only an eligible JavaScript-shell response.
    Auto,
}

/// One typed, policy-checked fetch operation.
///
/// Prefer [`FetchRequest::request`], [`FetchRequest::browser`], or
/// [`FetchRequest::auto`] over a struct literal. Debug output redacts the URL,
/// headers, proxy, and host restriction.
#[derive(Clone)]
pub struct FetchRequest {
    /// Credential-free HTTP(S) destination.
    pub url: Url,
    /// Selected request/browser behavior.
    pub mode: FetchMode,
    /// Additional request headers; diagnostics expose only the count.
    pub headers: HeaderMap,
    /// Optional validated HTTP or SOCKS proxy.
    pub proxy: Option<Url>,
    /// Optional destination allowlist applied to the initial URL and redirects.
    pub host_restriction: Option<FetchHostRestriction>,
}

impl fmt::Debug for FetchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FetchRequest")
            .field("url", &Redacted)
            .field("mode", &self.mode)
            .field("header_count", &self.headers.len())
            .field("proxy_configured", &self.proxy.is_some())
            .field(
                "host_restriction_configured",
                &self.host_restriction.is_some(),
            )
            .finish()
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// A redirect observed without requesting its target.
#[derive(Debug, Clone)]
pub struct FetchRedirect {
    pub status: u16,
    pub next_request: FetchRequest,
}

/// One HTTP document hop, stopping before a redirect target is requested.
#[derive(Debug)]
pub enum FetchStep {
    Redirect(FetchRedirect),
    Response(Page),
}

/// One bounded robots.txt hop with status exposed before document MIME checks.
pub enum RobotsFetchStep {
    Redirect(FetchRedirect),
    Missing { url: Url },
    Text { url: Url, text: String },
    Status { url: Url, status: u16 },
}

impl fmt::Debug for RobotsFetchStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redirect(redirect) => formatter.debug_tuple("Redirect").field(redirect).finish(),
            Self::Missing { .. } => formatter
                .debug_struct("Missing")
                .field("url", &Redacted)
                .finish(),
            Self::Text { text, .. } => formatter
                .debug_struct("Text")
                .field("url", &Redacted)
                .field("text_len", &text.len())
                .finish(),
            Self::Status { status, .. } => formatter
                .debug_struct("Status")
                .field("url", &Redacted)
                .field("status", status)
                .finish(),
        }
    }
}

impl FetchRequest {
    /// Parse a URL into a request-mode operation.
    pub fn request(url: &str) -> Result<Self> {
        Self::new(url, FetchMode::Request)
    }

    /// Parse a URL into a browser-required operation.
    pub fn browser(url: &str) -> Result<Self> {
        Self::new(url, FetchMode::Browser)
    }

    /// Parse a URL into an HTTP-first auto-render operation.
    pub fn auto(url: &str) -> Result<Self> {
        Self::new(url, FetchMode::Auto)
    }

    fn new(url: &str, mode: FetchMode) -> Result<Self> {
        let url = Url::parse(url).map_err(|_| Error::InvalidInput("invalid URL".into()))?;
        validate_url(&url, NetworkPolicy::AllowPrivate)?;
        Ok(Self {
            url,
            mode,
            headers: HeaderMap::new(),
            proxy: None,
            host_restriction: None,
        })
    }
}

/// Bounded bytes from a request-mode response.
///
/// Debug output redacts the URL, content-type value, and body bytes.
#[derive(Clone)]
pub struct RawResponse {
    /// Final validated response URL.
    pub url: Url,
    /// HTTP status code.
    pub status: u16,
    /// Parsed content-type value when present.
    pub content_type: Option<String>,
    /// Streamed response bytes, capped by [`OperationLimits::max_body_bytes`].
    pub bytes: Vec<u8>,
    /// Transport that produced the response.
    pub via: FetchVia,
    /// Numeric, non-secret rate-limit metadata.
    pub rate_limit: ResponseRateLimit,
}

impl fmt::Debug for RawResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawResponse")
            .field("url", &Redacted)
            .field("status", &self.status)
            .field("content_type_present", &self.content_type.is_some())
            .field("bytes_len", &self.bytes.len())
            .field("via", &self.via)
            .field("rate_limit", &self.rate_limit)
            .finish()
    }
}

/// Numeric rate-limit metadata captured from the final HTTP response.
///
/// This deliberately does not expose a raw header map: response cookies,
/// authorization challenges, and arbitrary provider headers may contain
/// secrets and are not part of the transport's public diagnostics contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResponseRateLimit {
    /// Parsed `Retry-After` seconds when the header is numeric.
    pub retry_after_secs: Option<u64>,
    /// Parsed remaining-request count.
    pub remaining: Option<u64>,
    /// Parsed reset Unix timestamp in seconds.
    pub reset_epoch_secs: Option<u64>,
}

impl ResponseRateLimit {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            retry_after_secs: numeric_header(headers, "retry-after"),
            remaining: numeric_header(headers, "x-ratelimit-remaining"),
            reset_epoch_secs: numeric_header(headers, "x-ratelimit-reset"),
        }
    }
}

/// Destination restriction carried through redirect requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchHostRestriction {
    allowed_hosts: Box<[String]>,
    kind: FetchHostRestrictionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchHostRestrictionKind {
    HttpsLabelSuffixes,
    HttpOrHttpsExactHost,
}

impl FetchHostRestriction {
    /// Permit HTTPS default-port hosts equal to or below one of these DNS-label
    /// suffixes.
    pub fn https_label_suffixes<I, S>(suffixes: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut allowed_suffixes = Vec::new();
        for suffix in suffixes {
            let suffix = normalize_domain_suffix(suffix.as_ref())?;
            if !allowed_suffixes.contains(&suffix) {
                allowed_suffixes.push(suffix);
            }
        }
        if allowed_suffixes.is_empty() {
            return Err(Error::InvalidInput(
                "at least one host suffix is required".into(),
            ));
        }
        Ok(Self {
            allowed_hosts: allowed_suffixes.into_boxed_slice(),
            kind: FetchHostRestrictionKind::HttpsLabelSuffixes,
        })
    }

    /// Restrict HTTP(S) requests to the exact normalized host of `url`.
    pub fn http_or_https_exact_host(url: &Url) -> Result<Self> {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::InvalidInput(
                "exact host restriction requires an HTTP(S) URL without credentials".into(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::InvalidInput("exact host restriction requires a host".into()))?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        Ok(Self {
            allowed_hosts: vec![host].into_boxed_slice(),
            kind: FetchHostRestrictionKind::HttpOrHttpsExactHost,
        })
    }

    /// Validate one URL against this restriction without network I/O.
    pub fn validate(&self, url: &Url) -> Result<()> {
        if self.kind == FetchHostRestrictionKind::HttpOrHttpsExactHost {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(Error::Policy(
                    "exact-host request must use HTTP or HTTPS".into(),
                ));
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(Error::Policy(
                    "restricted request credentials are not allowed".into(),
                ));
            }
            let host = url
                .host_str()
                .ok_or_else(|| Error::Policy("restricted request host is required".into()))?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if self
                .allowed_hosts
                .first()
                .is_some_and(|allowed| host == *allowed)
            {
                return Ok(());
            }
            return Err(Error::Policy(
                "restricted request host is not allowed".into(),
            ));
        }

        if url.scheme() != "https" {
            return Err(Error::Policy("restricted request must use HTTPS".into()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::Policy(
                "restricted request credentials are not allowed".into(),
            ));
        }
        if url.port_or_known_default() != Some(443) {
            return Err(Error::Policy(
                "restricted request must use the default HTTPS port".into(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::Policy("restricted request host is required".into()))?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if self
            .allowed_hosts
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        {
            return Ok(());
        }
        Err(Error::Policy(
            "restricted request host is not allowed".into(),
        ))
    }
}

#[derive(Debug, Clone, Copy)]
enum ResponseFormat {
    DecodedPage,
    RawBytes,
}

enum FetchResult {
    Page(Page),
    Raw(RawResponse),
}

enum HttpHop {
    Redirect(FetchRedirect),
    Response {
        url: Url,
        response: reqwest::Response,
    },
}

/// Reusable policy-enforcing fetch transport.
///
/// Clones share connection pools, immutable limits/policy, and an optional
/// browser backend.
#[derive(Clone)]
pub struct FetchClient {
    inner: Arc<FetchClientInner>,
}

struct FetchClientInner {
    clients: Mutex<ProxyClientPool>,
    limits: OperationLimits,
    policy: NetworkPolicy,
    resolver: Arc<dyn ResolverSource>,
    browser: Option<Arc<dyn BrowserBackend>>,
}

struct ProxyClientPool {
    direct: reqwest::Client,
    proxies: VecDeque<(ProxyKey, reqwest::Client)>,
}

#[derive(Clone, PartialEq, Eq)]
struct ProxyKey(Url);

impl ProxyClientPool {
    fn new(direct: reqwest::Client) -> Self {
        Self {
            direct,
            proxies: VecDeque::new(),
        }
    }

    fn direct(&self) -> reqwest::Client {
        self.direct.clone()
    }

    fn get_proxy(&mut self, key: &ProxyKey) -> Option<reqwest::Client> {
        let index = self
            .proxies
            .iter()
            .position(|(candidate, _)| candidate == key)?;
        let entry = self.proxies.remove(index)?;
        let client = entry.1.clone();
        self.proxies.push_back(entry);
        Some(client)
    }

    fn insert_proxy(&mut self, key: ProxyKey, client: reqwest::Client) {
        if self.proxies.len() == MAX_PROXY_CLIENTS {
            self.proxies.pop_front();
        }
        self.proxies.push_back((key, client));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        1 + self.proxies.len()
    }
}

impl FetchClient {
    /// Start a builder with public-network policy and default limits.
    pub fn builder() -> FetchClientBuilder {
        FetchClientBuilder::default()
    }

    /// Fetch a decoded [`Page`] according to the request mode.
    pub async fn fetch_request(&self, request: FetchRequest) -> Result<Page> {
        match request.mode {
            FetchMode::Request => self.fetch_http(request).await,
            FetchMode::Browser => self.render(request).await,
            FetchMode::Auto => self.fetch_auto(request).await,
        }
    }

    /// Fetch bounded raw bytes. Only request mode is accepted.
    pub async fn fetch_raw_request(&self, request: FetchRequest) -> Result<RawResponse> {
        if request.mode != FetchMode::Request {
            return Err(Error::InvalidInput(
                "raw fetch only supports request mode".into(),
            ));
        }
        self.fetch_raw_http(request).await
    }

    /// Validate all request properties that can be checked without DNS or I/O.
    ///
    /// The crawler uses this before allocating channels or spawning its
    /// scheduler, so a known-incompatible proxy/destination pairing fails
    /// synchronously.
    pub fn preflight_request(&self, request: &FetchRequest) -> Result<()> {
        validate_url(&request.url, self.inner.policy)?;
        if let Some(restriction) = &request.host_restriction {
            restriction.validate(&request.url)?;
        }
        if let Some(proxy) = &request.proxy {
            validate_proxy(proxy, self.inner.policy)?;
            if self.inner.policy == NetworkPolicy::PublicInternet
                && matches!(request.url.host(), Some(Host::Domain(_)))
                && matches!(proxy.scheme(), "http" | "https" | "socks4a" | "socks5h")
            {
                return Err(Error::Policy(
                    "remote proxy DNS cannot enforce public destination policy".into(),
                ));
            }
        }
        Ok(())
    }

    /// Fetch exactly one request-mode document hop.
    ///
    /// Redirects return a prepared next request but never send it. Sensitive
    /// headers are stripped when the origin changes.
    pub async fn fetch_request_one_hop(&self, request: FetchRequest) -> Result<FetchStep> {
        let deadline = self.request_deadline()?;
        self.fetch_request_one_hop_until(request, deadline).await
    }

    pub(crate) async fn fetch_request_one_hop_until(
        &self,
        request: FetchRequest,
        deadline: Instant,
    ) -> Result<FetchStep> {
        if request.mode != FetchMode::Request {
            return Err(Error::InvalidInput(
                "one-hop fetch only supports request mode".into(),
            ));
        }
        ensure_deadline_ready(deadline)?;
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => Err(Error::Timeout {
                operation: "request",
            }),
            result = async {
                match self.send_http_hop(request).await? {
                    HttpHop::Redirect(redirect) => Ok(FetchStep::Redirect(redirect)),
                    HttpHop::Response { url, response } => {
                        match self
                            .response_from_response(url, response, ResponseFormat::DecodedPage)
                            .await?
                        {
                            FetchResult::Page(page) => Ok(FetchStep::Response(page)),
                            FetchResult::Raw(_) => unreachable!("decoded fetch returned raw bytes"),
                        }
                    }
                }
            } => result,
        }
    }

    /// Fetch exactly one robots.txt hop.
    ///
    /// A 404 is returned without validating or reading its body. Successful
    /// robots responses must be bounded `text/plain`; other statuses are
    /// exposed without treating their bodies as documents.
    pub async fn fetch_robots_one_hop(&self, request: FetchRequest) -> Result<RobotsFetchStep> {
        let deadline = self.request_deadline()?;
        self.fetch_robots_one_hop_until(request, deadline).await
    }

    pub(crate) async fn fetch_robots_one_hop_until(
        &self,
        request: FetchRequest,
        deadline: Instant,
    ) -> Result<RobotsFetchStep> {
        if request.mode != FetchMode::Request {
            return Err(Error::InvalidInput(
                "robots fetch only supports request mode".into(),
            ));
        }
        ensure_deadline_ready(deadline)?;
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => Err(Error::Timeout {
                operation: "request",
            }),
            result = async {
                match self.send_http_hop(request).await? {
                    HttpHop::Redirect(redirect) => Ok(RobotsFetchStep::Redirect(redirect)),
                    HttpHop::Response { url, response } => {
                        let status = response.status().as_u16();
                        if status == 404 {
                            return Ok(RobotsFetchStep::Missing { url });
                        }
                        if !(200..300).contains(&status) {
                            return Ok(RobotsFetchStep::Status { url, status });
                        }
                        let metadata = validate_robots_metadata(response.headers())?;
                        let bytes = self.read_bounded_body(response).await?;
                        Ok(RobotsFetchStep::Text {
                            url,
                            text: decode_body(&bytes, metadata.encoding),
                        })
                    }
                }
            } => result,
        }
    }

    pub(crate) fn request_deadline(&self) -> Result<Instant> {
        self.request_deadline_at(Instant::now())
    }

    fn request_deadline_at(&self, now: Instant) -> Result<Instant> {
        let timeout = self.inner.limits.request_timeout;
        validate_timeout("request", timeout, MAX_REQUEST_TIMEOUT)?;
        now.checked_add(timeout).ok_or_else(|| {
            Error::InvalidInput("request timeout deadline cannot be represented".into())
        })
    }

    async fn fetch_http(&self, request: FetchRequest) -> Result<Page> {
        match tokio::time::timeout(
            self.inner.limits.request_timeout,
            self.fetch_inner(request, ResponseFormat::DecodedPage),
        )
        .await
        .map_err(|_| Error::Timeout {
            operation: "request",
        })?? {
            FetchResult::Page(page) => Ok(page),
            FetchResult::Raw(_) => unreachable!("decoded fetch returned raw bytes"),
        }
    }

    async fn fetch_raw_http(&self, request: FetchRequest) -> Result<RawResponse> {
        match tokio::time::timeout(
            self.inner.limits.request_timeout,
            self.fetch_inner(request, ResponseFormat::RawBytes),
        )
        .await
        .map_err(|_| Error::Timeout {
            operation: "request",
        })?? {
            FetchResult::Raw(response) => Ok(response),
            FetchResult::Page(_) => unreachable!("raw fetch returned decoded page"),
        }
    }

    async fn fetch_auto(&self, request: FetchRequest) -> Result<Page> {
        let page = self.fetch_http(request.clone()).await?;
        if !page_is_render_eligible(&page) || !looks_like_javascript_shell(&page.html) {
            return Ok(page);
        }
        let Some(browser) = &self.inner.browser else {
            return Ok(page);
        };
        match browser.render(&request, &self.inner.limits).await {
            Ok(rendered) => Ok(rendered),
            Err(_) => Ok(page),
        }
    }

    async fn render(&self, request: FetchRequest) -> Result<Page> {
        validate_url(&request.url, self.inner.policy)?;
        if let Some(restriction) = &request.host_restriction {
            restriction.validate(&request.url)?;
        }
        let browser = self
            .inner
            .browser
            .as_ref()
            .ok_or_else(|| Error::Browser("browser backend is not configured".into()))?;
        browser.render(&request, &self.inner.limits).await
    }

    async fn fetch_inner(
        &self,
        request: FetchRequest,
        response_format: ResponseFormat,
    ) -> Result<FetchResult> {
        let mut request = request;
        let mut redirects = 0usize;

        loop {
            match self.send_http_hop(request).await? {
                HttpHop::Redirect(redirect) => {
                    if redirects >= self.inner.limits.max_redirects {
                        return Err(Error::Policy("redirect limit exceeded".into()));
                    }
                    request = redirect.next_request;
                    redirects += 1;
                }
                HttpHop::Response { url, response } => {
                    return self
                        .response_from_response(url, response, response_format)
                        .await;
                }
            }
        }
    }

    async fn send_http_hop(&self, request: FetchRequest) -> Result<HttpHop> {
        self.preflight_request(&request)?;
        let client = self.client_for_proxy(request.proxy.as_ref())?;
        if let Some(proxy) = &request.proxy {
            self.validate_proxied_destination(&request.url, proxy)
                .await?;
        }

        let response = client
            .get(request.url.clone())
            .headers(request.headers.clone())
            .send()
            .await
            .map_err(map_transport_error)?;

        if is_followed_redirect(response.status().as_u16()) {
            if let Some(location) = response.headers().get(LOCATION) {
                let location = location
                    .to_str()
                    .map_err(|_| Error::Policy("redirect location is not valid text".into()))?;
                let target = request
                    .url
                    .join(location)
                    .map_err(|_| Error::Policy("redirect location is not a valid URL".into()))?;
                redirect_target_is_allowed(
                    self.inner.policy,
                    request.host_restriction.as_ref(),
                    &request.url,
                    &target,
                )?;
                let status = response.status().as_u16();
                let mut next_request = request;
                if !same_origin(&next_request.url, &target) {
                    strip_sensitive_headers(&mut next_request.headers);
                }
                next_request.url = target;
                self.preflight_request(&next_request)?;
                return Ok(HttpHop::Redirect(FetchRedirect {
                    status,
                    next_request,
                }));
            }
        }

        Ok(HttpHop::Response {
            url: request.url,
            response,
        })
    }

    async fn response_from_response(
        &self,
        displayed_url: Url,
        response: reqwest::Response,
        response_format: ResponseFormat,
    ) -> Result<FetchResult> {
        let status = response.status().as_u16();
        let metadata = validate_response_metadata(response.headers())?;
        let rate_limit = ResponseRateLimit::from_headers(response.headers());

        let bytes = self.read_bounded_body(response).await?;

        match response_format {
            ResponseFormat::DecodedPage => {
                let html = decode_body(&bytes, metadata.encoding);
                Ok(FetchResult::Page(Page {
                    url: displayed_url,
                    status,
                    content_type: Some(metadata.declaration),
                    html,
                    via: FetchVia::Request,
                }))
            }
            ResponseFormat::RawBytes => Ok(FetchResult::Raw(RawResponse {
                url: displayed_url,
                status,
                content_type: Some(metadata.declaration),
                bytes,
                via: FetchVia::Request,
                rate_limit,
            })),
        }
    }

    async fn read_bounded_body(&self, response: reqwest::Response) -> Result<Vec<u8>> {
        if response
            .content_length()
            .is_some_and(|length| length > self.inner.limits.max_body_bytes as u64)
        {
            return Err(Error::BodyLimit {
                limit: self.inner.limits.max_body_bytes,
            });
        }

        let mut bytes = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or_default()
                .min(self.inner.limits.max_body_bytes as u64) as usize,
        );
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_transport_error)?;
            if chunk.len() > self.inner.limits.max_body_bytes.saturating_sub(bytes.len()) {
                return Err(Error::BodyLimit {
                    limit: self.inner.limits.max_body_bytes,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn validate_proxied_destination(&self, url: &Url, proxy: &Url) -> Result<()> {
        if self.inner.policy == NetworkPolicy::AllowPrivate {
            return Ok(());
        }
        let Host::Domain(host) = url
            .host()
            .ok_or_else(|| Error::Policy("URL host is required".into()))?
        else {
            return Ok(());
        };
        let addresses = self
            .inner
            .resolver
            .resolve(host.to_owned())
            .await
            .map_err(|_| Error::Dns("destination resolution failed".into()))?;
        if addresses.is_empty() {
            return Err(Error::Dns(
                "destination resolution returned no addresses".into(),
            ));
        }
        if addresses
            .into_iter()
            .any(|address| !address_is_allowed(NetworkPolicy::PublicInternet, address.ip()))
        {
            return Err(Error::Policy(
                "proxied destination resolved to a forbidden address".into(),
            ));
        }
        if matches!(proxy.scheme(), "http" | "https" | "socks4a" | "socks5h") {
            return Err(Error::Policy(
                "remote proxy DNS cannot enforce public destination policy".into(),
            ));
        }
        Ok(())
    }

    fn client_for_proxy(&self, proxy: Option<&Url>) -> Result<reqwest::Client> {
        if let Some(proxy) = proxy {
            validate_proxy(proxy, self.inner.policy)?;
        }
        let mut clients = self.inner.clients.lock().map_err(|_| Error::Cancelled)?;
        let Some(proxy) = proxy else {
            return Ok(clients.direct());
        };
        let key = ProxyKey(proxy.clone());
        if let Some(client) = clients.get_proxy(&key) {
            return Ok(client);
        }
        let client = build_transport(
            &self.inner.limits,
            self.inner.policy,
            Arc::clone(&self.inner.resolver),
            Some(proxy),
        )?;
        clients.insert_proxy(key, client.clone());
        Ok(client)
    }

    /// Limits fixed when this client was built.
    pub fn limits(&self) -> &OperationLimits {
        &self.inner.limits
    }

    /// Network policy fixed when this client was built.
    pub fn policy(&self) -> NetworkPolicy {
        self.inner.policy
    }
}

/// Builder for one [`FetchClient`] policy domain.
pub struct FetchClientBuilder {
    policy: NetworkPolicy,
    limits: OperationLimits,
    resolver: Arc<dyn ResolverSource>,
    browser: Option<Arc<dyn BrowserBackend>>,
}

impl Default for FetchClientBuilder {
    fn default() -> Self {
        Self {
            policy: NetworkPolicy::default(),
            limits: OperationLimits::default(),
            resolver: Arc::new(SystemResolver),
            browser: None,
        }
    }
}

impl FetchClientBuilder {
    /// Select destination policy; private access is an explicit trusted opt-in.
    pub fn policy(mut self, policy: NetworkPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replace transport and output limits.
    pub fn limits(mut self, limits: OperationLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replace DNS lookup for an embedder or deterministic fixture.
    pub fn resolver(mut self, resolver: Arc<dyn ResolverSource>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Register the backend used by browser and eligible auto requests.
    pub fn browser(mut self, browser: Arc<dyn BrowserBackend>) -> Self {
        self.browser = Some(browser);
        self
    }

    /// Validate fixed limits and construct the reusable client.
    pub fn build(self) -> Result<FetchClient> {
        validate_timeout("connect", self.limits.connect_timeout, MAX_CONNECT_TIMEOUT)?;
        validate_timeout("request", self.limits.request_timeout, MAX_REQUEST_TIMEOUT)?;
        let client = build_transport(&self.limits, self.policy, Arc::clone(&self.resolver), None)?;
        Ok(FetchClient {
            inner: Arc::new(FetchClientInner {
                clients: Mutex::new(ProxyClientPool::new(client)),
                limits: self.limits,
                policy: self.policy,
                resolver: self.resolver,
                browser: self.browser,
            }),
        })
    }
}

fn validate_timeout(
    name: &str,
    timeout: std::time::Duration,
    maximum: std::time::Duration,
) -> Result<()> {
    if timeout.is_zero() || timeout > maximum {
        return Err(Error::InvalidInput(format!(
            "{name} timeout must be positive and no greater than {maximum:?}"
        )));
    }
    Ok(())
}

fn ensure_deadline_ready(deadline: Instant) -> Result<()> {
    if deadline <= Instant::now() {
        return Err(Error::Timeout {
            operation: "request",
        });
    }
    Ok(())
}

fn page_is_render_eligible(page: &Page) -> bool {
    (200..300).contains(&page.status)
        && page.content_type.as_deref().is_some_and(|content_type| {
            let media_type = content_type.split(';').next().unwrap_or_default().trim();
            media_type.eq_ignore_ascii_case("text/html")
                || media_type.eq_ignore_ascii_case("application/xhtml+xml")
        })
}

fn build_transport(
    limits: &OperationLimits,
    policy: NetworkPolicy,
    resolver: Arc<dyn ResolverSource>,
    proxy: Option<&Url>,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .tls_backend_rustls()
        .connect_timeout(limits.connect_timeout)
        .timeout(limits.request_timeout)
        .user_agent(DEFAULT_UA)
        .dns_resolver(SafeResolver::new(resolver, policy))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if validate_url(attempt.url(), policy).is_err() {
                attempt.error(PolicyDnsError::Redirect)
            } else {
                attempt.stop()
            }
        }));
    if let Some(proxy) = proxy {
        let explicit = reqwest::Proxy::all(proxy.as_str())
            .map_err(|_| Error::InvalidInput("invalid proxy URL".into()))?;
        builder = builder.proxy(explicit);
    }
    builder.build().map_err(|error| {
        if proxy.is_some() {
            Error::InvalidInput("failed to build proxy transport".into())
        } else {
            map_transport_error(error)
        }
    })
}

fn validate_proxy(proxy: &Url, policy: NetworkPolicy) -> Result<()> {
    if !matches!(
        proxy.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) {
        return Err(Error::InvalidInput("unsupported proxy scheme".into()));
    }
    let host = proxy
        .host()
        .ok_or_else(|| Error::InvalidInput("proxy host is required".into()))?;
    let literal_address = match host {
        Host::Ipv4(address) => Some(IpAddr::V4(address)),
        Host::Ipv6(address) => Some(IpAddr::V6(address)),
        Host::Domain(domain) => domain.parse().ok(),
    };
    if literal_address.is_some_and(|address| !address_is_allowed(policy, address)) {
        return Err(Error::Policy(
            "proxy endpoint is outside the permitted network policy".into(),
        ));
    }
    if matches!(proxy.scheme(), "socks4" | "socks4a" | "socks5" | "socks5h")
        && literal_address.is_none()
    {
        return Err(Error::InvalidInput(
            "SOCKS proxy host must be an IP address".into(),
        ));
    }
    if proxy.query().is_some() || proxy.fragment().is_some() {
        return Err(Error::InvalidInput(
            "proxy URL cannot contain a query or fragment".into(),
        ));
    }
    Ok(())
}

fn redirect_target_is_allowed(
    policy: NetworkPolicy,
    host_restriction: Option<&FetchHostRestriction>,
    _start: &Url,
    target: &Url,
) -> Result<()> {
    validate_url(target, policy)?;
    if let Some(restriction) = host_restriction {
        restriction.validate(target)?;
    }
    Ok(())
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn normalize_domain_suffix(suffix: &str) -> Result<String> {
    let suffix = suffix.trim().trim_end_matches('.').to_ascii_lowercase();
    let valid = !suffix.is_empty()
        && suffix.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        });
    if valid {
        Ok(suffix)
    } else {
        Err(Error::InvalidInput("invalid host suffix".into()))
    }
}

fn strip_sensitive_headers(headers: &mut HeaderMap) {
    headers.remove(AUTHORIZATION);
    headers.remove(COOKIE);
    headers.remove("cookie2");
    headers.remove(PROXY_AUTHORIZATION);
    headers.remove(WWW_AUTHENTICATE);
    let marked_sensitive: Vec<_> = headers
        .iter()
        .filter(|(_, value)| value.is_sensitive())
        .map(|(name, _)| name.clone())
        .collect();
    for name in marked_sensitive {
        headers.remove(name);
    }
}

fn is_followed_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn numeric_header(headers: &HeaderMap, name: &'static str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn validate_response_metadata(headers: &HeaderMap) -> Result<ValidatedContentType> {
    let dispositions = headers
        .get_all(CONTENT_DISPOSITION)
        .iter()
        .map(|value| {
            value
                .to_str()
                .map_err(|_| Error::Policy("invalid response content disposition".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    reject_attachments(&dispositions)?;
    let declarations = headers
        .get_all(CONTENT_TYPE)
        .iter()
        .map(|value| {
            value
                .to_str()
                .map_err(|_| Error::Policy("invalid response content type".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let validated = validate_content_type_declarations(&declarations)?;
    if !validated.identity.media_type.is_supported_http() {
        return Err(Error::Policy("unsupported response content type".into()));
    }
    Ok(validated)
}

fn validate_robots_metadata(headers: &HeaderMap) -> Result<ValidatedContentType> {
    let metadata = validate_response_metadata(headers)?;
    let media_type = metadata
        .declaration
        .split(';')
        .next()
        .unwrap_or_default()
        .trim();
    if !media_type.eq_ignore_ascii_case("text/plain") {
        return Err(Error::Policy(
            "robots.txt must use a text/plain content type".into(),
        ));
    }
    Ok(metadata)
}

fn decode_body(bytes: &[u8], encoding: &'static Encoding) -> String {
    let (decoded, _, _) = encoding.decode(bytes);
    decoded.into_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        is_followed_redirect, redirect_target_is_allowed, FetchClient, FetchHostRestriction,
        FetchRequest, ProxyClientPool, ProxyKey, MAX_PROXY_CLIENTS,
    };
    use crate::spider::{CrawlConfig, Crawler};
    use crate::{Error, NetworkPolicy, OperationLimits, ResolverSource};
    use std::future::Future;
    use std::io;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use url::Url;

    #[test]
    fn public_redirect_to_a_private_target_is_rejected() {
        let start = Url::parse("https://93.184.216.34/start").unwrap();
        let target = Url::parse("http://127.0.0.1/private").unwrap();
        assert!(
            redirect_target_is_allowed(NetworkPolicy::PublicInternet, None, &start, &target)
                .is_err()
        );
    }

    #[test]
    fn host_restriction_url_probes_require_https_default_port_and_label_suffixes() {
        let restriction = FetchHostRestriction::https_label_suffixes([
            "youtube.com",
            "youtube-nocookie.com",
            "google.com",
            "googlevideo.com",
        ])
        .unwrap();

        for allowed in [
            "https://youtube.com/api/timedtext",
            "https://www.youtube.com/api/timedtext",
            "https://rr1---sn.googlevideo.com/videoplayback",
            "https://www.youtube.com:443/api/timedtext",
        ] {
            restriction.validate(&Url::parse(allowed).unwrap()).unwrap();
        }

        for blocked in [
            "http://www.youtube.com/api/timedtext",
            "https://www.youtube.com:444/api/timedtext",
            "https://user:pass@www.youtube.com/api/timedtext",
            "https://www.youtube.com.evil.example/api/timedtext",
            "https://youtube.com.evil/api/timedtext",
        ] {
            let error = restriction
                .validate(&Url::parse(blocked).unwrap())
                .unwrap_err();
            assert!(matches!(error, Error::Policy(_)), "{blocked}: {error}");
        }
    }

    #[test]
    fn redirect_target_policy_applies_request_host_restriction() {
        let restriction =
            FetchHostRestriction::https_label_suffixes(["youtube.com", "googlevideo.com"]).unwrap();
        let start = Url::parse("https://www.youtube.com/api/timedtext").unwrap();
        let allowed = Url::parse("https://rr1---sn.googlevideo.com/videoplayback").unwrap();
        let blocked = Url::parse("https://evil.example/steal").unwrap();

        redirect_target_is_allowed(
            NetworkPolicy::PublicInternet,
            Some(&restriction),
            &start,
            &allowed,
        )
        .unwrap();
        let error = redirect_target_is_allowed(
            NetworkPolicy::PublicInternet,
            Some(&restriction),
            &start,
            &blocked,
        )
        .unwrap_err();

        assert!(matches!(error, Error::Policy(_)), "{error}");
    }

    #[test]
    fn only_rfc_follow_redirect_statuses_are_followed() {
        for status in 300..400 {
            assert_eq!(
                is_followed_redirect(status),
                matches!(status, 301 | 302 | 303 | 307 | 308),
                "status {status}"
            );
        }
    }

    #[tokio::test]
    async fn proxied_redirect_destination_rejects_a_private_dns_answer() {
        let client = FetchClient::builder()
            .resolver(Arc::new(FixtureResolver::new("127.0.0.1")))
            .build()
            .unwrap();

        let error = client
            .validate_proxied_destination(
                &Url::parse("http://redirected.test/resource").unwrap(),
                &Url::parse("http://93.184.216.34:8080/").unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Policy(_)));
    }

    #[tokio::test]
    async fn public_hostname_destination_rejects_remote_proxy_dns() {
        let client = FetchClient::builder()
            .resolver(Arc::new(FixtureResolver::new("93.184.216.34")))
            .build()
            .unwrap();

        let error = client
            .validate_proxied_destination(
                &Url::parse("http://public.test/resource").unwrap(),
                &Url::parse("http://93.184.216.34:8080/").unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Policy(_)));
    }

    #[tokio::test]
    async fn failed_credential_distinct_proxies_cannot_grow_the_client_pool_without_bound() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let dropping_proxy = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let client = FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .limits(OperationLimits {
                connect_timeout: Duration::from_millis(100),
                request_timeout: Duration::from_millis(250),
                ..OperationLimits::default()
            })
            .build()
            .unwrap();

        for index in 0..12 {
            let mut request = FetchRequest::request("http://93.184.216.34/resource").unwrap();
            request.proxy = Some(
                Url::parse(&format!(
                    "http://user-{index}:credential-{index}@{proxy_address}/"
                ))
                .unwrap(),
            );
            let error = client.fetch_request(request).await.unwrap_err();
            let diagnostic = format!("{error:?} {error}");
            assert!(!diagnostic.contains(&format!("credential-{index}")));
        }

        let retained = client.inner.clients.lock().unwrap().len();
        assert!(retained <= 9, "retained {retained} clients");
        dropping_proxy.abort();
    }

    #[test]
    fn proxy_client_pool_evicts_the_least_recently_used_entry() {
        let transport = reqwest::Client::new();
        let mut pool = ProxyClientPool::new(transport.clone());
        let key = |index| ProxyKey(Url::parse(&format!("http://proxy-{index}.test/")).unwrap());
        for index in 0..MAX_PROXY_CLIENTS {
            pool.insert_proxy(key(index), transport.clone());
        }

        assert!(pool.get_proxy(&key(0)).is_some());
        pool.insert_proxy(key(MAX_PROXY_CLIENTS), transport);

        assert!(pool.get_proxy(&key(1)).is_none());
        assert!(pool.get_proxy(&key(0)).is_some());
        assert!(pool.get_proxy(&key(MAX_PROXY_CLIENTS)).is_some());
    }

    #[tokio::test]
    async fn already_expired_page_and_robots_hops_never_poll_transport_io() {
        let port = 9;
        let resolver = Arc::new(CountingResolver::new("127.0.0.1"));
        let client = FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .resolver(resolver.clone())
            .limits(OperationLimits {
                connect_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_secs(1),
                ..OperationLimits::default()
            })
            .build()
            .unwrap();

        for repetition in 0..50 {
            let deadline = tokio::time::Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap();
            for (kind, robots) in [
                ("initial-page", false),
                ("page-redirect", false),
                ("initial-robots", true),
                ("robots-redirect", true),
            ] {
                let request = FetchRequest::request(&format!(
                    "http://{kind}-{repetition}.test:{port}/{kind}"
                ))
                .unwrap();
                let result = if robots {
                    client
                        .fetch_robots_one_hop_until(request, deadline)
                        .await
                        .map(|_| ())
                } else {
                    client
                        .fetch_request_one_hop_until(request, deadline)
                        .await
                        .map(|_| ())
                };
                assert!(
                    matches!(
                        result,
                        Err(Error::Timeout {
                            operation: "request"
                        })
                    ),
                    "{kind} repetition {repetition}: {result:?}"
                );
            }
        }

        // An exact-now deadline is another valid near-zero boundary and must
        // neither panic nor get one eager transport poll.
        let request = FetchRequest::request(&format!("http://exact-now.test:{port}/page")).unwrap();
        let result = client
            .fetch_request_one_hop_until(request, tokio::time::Instant::now())
            .await;
        assert!(matches!(result, Err(Error::Timeout { .. })));
        tokio::task::yield_now().await;
        assert_eq!(resolver.calls(), 0, "expired hops reached DNS resolution");
    }

    #[tokio::test]
    async fn public_one_hop_methods_reject_an_internal_invalid_timeout_without_io() {
        for (kind, robots) in [("page", false), ("robots", true)] {
            let resolver = Arc::new(CountingResolver::new("127.0.0.1"));
            let mut client = FetchClient::builder()
                .policy(NetworkPolicy::AllowPrivate)
                .resolver(resolver.clone())
                .build()
                .unwrap();
            Arc::get_mut(&mut client.inner)
                .expect("test owns the only client instance")
                .limits
                .request_timeout = Duration::MAX;
            let request =
                FetchRequest::request(&format!("http://internal-invalid-{kind}.test:9/{kind}"))
                    .unwrap();

            let result = if robots {
                client.fetch_robots_one_hop(request).await.map(|_| ())
            } else {
                client.fetch_request_one_hop(request).await.map(|_| ())
            };

            assert!(
                matches!(result, Err(Error::InvalidInput(_))),
                "{kind} one-hop call returned {result:?}"
            );
            tokio::task::yield_now().await;
            assert_eq!(resolver.calls(), 0, "{kind} invalid timeout reached DNS");
        }
    }

    #[test]
    fn deadline_creation_is_fallible_at_an_injected_later_near_instant_limit() {
        fn largest_representable_whole_seconds(now: tokio::time::Instant) -> u64 {
            let mut lower = 0u64;
            let mut upper = u64::MAX;
            while lower < upper {
                let middle = lower + (upper - lower) / 2 + 1;
                if now.checked_add(Duration::from_secs(middle)).is_some() {
                    lower = middle;
                } else {
                    upper = middle - 1;
                }
            }
            lower
        }

        let client = FetchClient::builder()
            .limits(OperationLimits {
                request_timeout: crate::limits::MAX_REQUEST_TIMEOUT,
                ..OperationLimits::default()
            })
            .build()
            .unwrap();
        let constructed_at = tokio::time::Instant::now();
        let ordinary_later = constructed_at.checked_add(Duration::from_secs(1)).unwrap();
        let ordinary_deadline = client.request_deadline_at(ordinary_later).unwrap();
        assert_eq!(
            ordinary_deadline.duration_since(ordinary_later),
            crate::limits::MAX_REQUEST_TIMEOUT
        );

        let elapsed_seconds = largest_representable_whole_seconds(constructed_at);
        let near_limit_later = constructed_at
            .checked_add(Duration::from_secs(elapsed_seconds))
            .unwrap();
        let error = client.request_deadline_at(near_limit_later).unwrap_err();
        assert!(matches!(error, Error::InvalidInput(_)), "{error}");
    }

    #[tokio::test]
    async fn crawler_rejects_an_internal_invalid_timeout_before_spawn_or_io() {
        let resolver = Arc::new(CountingResolver::new("127.0.0.1"));
        let mut client = FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .resolver(resolver.clone())
            .build()
            .unwrap();
        Arc::get_mut(&mut client.inner)
            .expect("test owns the only client instance")
            .limits
            .request_timeout = Duration::MAX;
        let crawler = Crawler::new(client);
        let config = CrawlConfig {
            start_url: Url::parse("http://internal-invalid-crawler.test:9/").unwrap(),
            max_pages: 1,
            concurrency: 1,
            same_origin_only: true,
            include_subdomains: false,
            respect_robots: false,
            minimum_delay: Duration::ZERO,
            proxies: Vec::new(),
        };

        let error = match crawler.stream(config) {
            Err(error) => error,
            Ok((stream, control, _)) => {
                control.cancel();
                drop(stream);
                panic!("crawler accepted an internal invalid request timeout")
            }
        };

        assert!(matches!(error, Error::InvalidInput(_)), "{error}");
        tokio::task::yield_now().await;
        assert_eq!(resolver.calls(), 0, "invalid crawler timeout reached DNS");
    }

    struct FixtureResolver {
        address: SocketAddr,
    }

    struct CountingResolver {
        address: SocketAddr,
        calls: AtomicUsize,
    }

    impl CountingResolver {
        fn new(address: &str) -> Self {
            Self {
                address: SocketAddr::new(address.parse().unwrap(), 0),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl FixtureResolver {
        fn new(address: &str) -> Self {
            Self {
                address: SocketAddr::new(address.parse().unwrap(), 0),
            }
        }
    }

    impl ResolverSource for FixtureResolver {
        fn resolve(
            &self,
            _host: String,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>> {
            let address = self.address;
            Box::pin(async move { Ok(vec![address]) })
        }
    }

    impl ResolverSource for CountingResolver {
        fn resolve(
            &self,
            _host: String,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let address = self.address;
            Box::pin(async move { Ok(vec![address]) })
        }
    }
}
