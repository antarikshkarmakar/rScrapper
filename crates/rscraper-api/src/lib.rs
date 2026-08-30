//! Bounded, authenticated HTTP routes for rScrapper 0.2.
//!
//! Production configuration defaults to `127.0.0.1:8787`; a non-loopback
//! listener requires a bearer token before bind. Strict JSON operation routes
//! share [`ApiState::operation_limit`], enforce route deadlines and
//! request/response caps, and retain the public-network policy in
//! [`AppContext`]. Remote output remains untrusted data.

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Json, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use rscraper_cli::context::AppContext;
use rscraper_cli::web::{self, SearchEndpoints, DEFAULT_SEARCH_RESULTS};
use rscraper_core::markdown::{html_to_markdown_with_options, MarkdownOptions};
use rscraper_core::{CrawlConfig, Crawler, Error, FetchRequest};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::trace::TraceLayer;
use url::Url;

const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const SCRAPE_SEARCH_DEADLINE: Duration = Duration::from_secs(30);
const CRAWL_DEADLINE: Duration = Duration::from_secs(120);
const DEFAULT_PORT: u16 = 8787;
const DEFAULT_MAX_CONCURRENT_OPERATIONS: usize = 8;
const MAX_CONCURRENT_OPERATIONS: usize = 32;
const DEFAULT_CRAWL_PAGES: usize = 20;
const MAX_CRAWL_PAGES: usize = 100;
const DEFAULT_CRAWL_CONCURRENCY: usize = 4;
const MAX_CRAWL_CONCURRENCY: usize = 16;

/// Shared state for one API router.
#[derive(Clone)]
pub struct ApiState {
    /// Policy-enforcing platform services.
    pub context: AppContext,
    /// Optional bearer token. When present it protects every operation route.
    pub token: Option<Arc<str>>,
    /// Nonblocking global operation admission gate.
    pub operation_limit: Arc<Semaphore>,
}

impl fmt::Debug for ApiState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiState")
            .field("context", &"<shared>")
            .field("token_configured", &self.token.is_some())
            .field(
                "available_operation_permits",
                &self.operation_limit.available_permits(),
            )
            .finish()
    }
}

/// Validated process startup configuration.
#[derive(Clone)]
pub struct ServerConfig {
    /// Socket address to bind.
    pub bind: SocketAddr,
    /// Optional bearer token; required when `bind` is not loopback.
    pub token: Option<String>,
    /// Operation permits, from 1 through 32.
    pub max_concurrent_operations: usize,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("bind", &self.bind)
            .field("token_configured", &self.token.is_some())
            .field("max_concurrent_operations", &self.max_concurrent_operations)
            .finish()
    }
}

impl ServerConfig {
    /// Parse configuration from the process environment without treating
    /// non-Unicode values as absent.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_os_lookup(|name| std::env::var_os(name))
    }

    /// Parse startup configuration from a supplied source.
    ///
    /// This is public to let embedders and tests parse configuration without
    /// mutating process-global environment variables.
    pub fn from_lookup<F>(lookup: F) -> anyhow::Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::from_values(
            lookup("RSCRAPER_BIND"),
            lookup("PORT"),
            lookup("RSCRAPER_API_TOKEN"),
            lookup("RSCRAPER_API_MAX_CONCURRENT_OPERATIONS"),
        )
    }

    /// Parse startup configuration from an OS-string source without silently
    /// treating non-Unicode values as absent.
    #[doc(hidden)]
    pub fn from_os_lookup<F>(lookup: F) -> anyhow::Result<Self>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let text = |name| {
            lookup(name)
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("{name} must be valid Unicode text"))
                })
                .transpose()
        };
        Self::from_values(
            text("RSCRAPER_BIND")?,
            text("PORT")?,
            text("RSCRAPER_API_TOKEN")?,
            text("RSCRAPER_API_MAX_CONCURRENT_OPERATIONS")?,
        )
    }

    fn from_values(
        bind: Option<String>,
        port: Option<String>,
        token: Option<String>,
        max_concurrent_operations: Option<String>,
    ) -> anyhow::Result<Self> {
        let bind = if let Some(bind) = bind {
            bind.parse::<SocketAddr>()
                .map_err(|_| anyhow::anyhow!("RSCRAPER_BIND must be a socket address"))?
        } else {
            let port = match port {
                Some(port) => port
                    .parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("PORT must be an integer from 0 through 65535"))?,
                None => DEFAULT_PORT,
            };
            SocketAddr::from(([127, 0, 0, 1], port))
        };
        let max_concurrent_operations = match max_concurrent_operations {
            Some(limit) => limit.parse::<usize>().map_err(|_| {
                anyhow::anyhow!(
                    "RSCRAPER_API_MAX_CONCURRENT_OPERATIONS must be an integer from 1 through 32"
                )
            })?,
            None => DEFAULT_MAX_CONCURRENT_OPERATIONS,
        };
        let config = Self {
            bind,
            token,
            max_concurrent_operations,
        };
        validate_server_config(&config)?;
        Ok(config)
    }
}

/// Enforce bind, token, and concurrency startup invariants.
pub fn validate_server_config(config: &ServerConfig) -> anyhow::Result<()> {
    if let Some(token) = &config.token {
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            anyhow::bail!(
                "RSCRAPER_API_TOKEN must contain only non-whitespace visible ASCII characters"
            );
        }
    }
    if !config.bind.ip().is_loopback() && config.token.is_none() {
        anyhow::bail!("a non-loopback bind requires RSCRAPER_API_TOKEN");
    }
    if !(1..=MAX_CONCURRENT_OPERATIONS).contains(&config.max_concurrent_operations) {
        anyhow::bail!("maximum concurrent operations must be between 1 and 32");
    }
    Ok(())
}

/// Replace Rust's payload-bearing default panic hook with a generic server
/// diagnostic. The catch-panic middleware still turns request panics into a
/// stable JSON response.
#[doc(hidden)]
pub fn install_redacted_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        tracing::error!("server task panicked");
    }));
}

#[derive(Clone)]
struct RuntimeState {
    api: ApiState,
    search_endpoints: SearchEndpoints,
}

/// Build the production router with fixed provider endpoints.
pub fn router(state: ApiState) -> Router {
    router_with_search_endpoints(state, SearchEndpoints::default())
}

/// Build the production router with server-owned search endpoints.
///
/// The ordinary server uses [`router`]. This narrow injection point keeps
/// local integration fixtures deterministic while still delegating all work
/// to the typed CLI service and the supplied policy-enforcing context.
#[doc(hidden)]
pub fn router_with_search_endpoints(state: ApiState, search_endpoints: SearchEndpoints) -> Router {
    let runtime = RuntimeState {
        api: state,
        search_endpoints,
    };
    let thirty_second_operations = Router::new()
        .route("/scrape", post(scrape))
        .route("/scrape/", post(scrape))
        .route("/search", post(search))
        .route("/search/", post(search))
        .layer(middleware::from_fn(thirty_second_deadline));
    let crawl_operation = Router::new()
        .route("/crawl", post(crawl))
        .route("/crawl/", post(crawl))
        .layer(middleware::from_fn(crawl_deadline));
    let request_id = HeaderName::from_static("x-request-id");
    let auth_state = runtime.clone();

    Router::new()
        .route("/health", get(health))
        .route("/health/", get(health))
        .merge(thirty_second_operations)
        .merge(crawl_operation)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(runtime)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            auth_state,
            authenticate_operation_path,
        ))
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(request_id.clone(), MakeRequestUuid))
                .layer(PropagateRequestIdLayer::new(request_id))
                .layer(SetSensitiveRequestHeadersLayer::new(std::iter::once(
                    AUTHORIZATION,
                )))
                .layer(TraceLayer::new_for_http().make_span_with(
                    |request: &axum::http::Request<Body>| {
                        tracing::info_span!("http_request", method = %request.method())
                    },
                ))
                .layer(CatchPanicLayer::custom(panic_response)),
        )
}

/// Serve a pre-bound listener until the supplied graceful-shutdown signal.
pub async fn serve_with_shutdown<F>(
    listener: TcpListener,
    state: ApiState,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

async fn health() -> Response {
    bounded_json(
        StatusCode::OK,
        &serde_json::json!({"status":"ok", "service":"rscraper-api"}),
    )
    .unwrap_or_else(IntoResponse::into_response)
}

async fn scrape(
    State(runtime): State<RuntimeState>,
    payload: Result<Json<ScrapeRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let request = extract_json(payload)?.0;
    let url = request.url.ok_or_else(ApiError::invalid_request)?;
    let preflight = FetchRequest::auto(&url).map_err(ApiError::from_core)?;
    runtime
        .api
        .context
        .fetch
        .preflight_request(&preflight)
        .map_err(ApiError::from_core)?;
    let _permit = acquire_operation(&runtime.api)?;
    let response = web::read(&runtime.api.context, &url)
        .await
        .map_err(ApiError::from_core)?;
    bounded_json(StatusCode::OK, &response)
}

async fn search(
    State(runtime): State<RuntimeState>,
    payload: Result<Json<SearchRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let request = extract_json(payload)?.0;
    let query = request.query.ok_or_else(ApiError::invalid_request)?;
    web::validate_search_input(&query, request.n).map_err(ApiError::from_core)?;
    let _permit = acquire_operation(&runtime.api)?;
    let response = web::search_with_endpoints(
        &runtime.api.context,
        &query,
        request.n,
        request.scrape,
        &runtime.search_endpoints,
    )
    .await
    .map_err(ApiError::from_core)?;
    bounded_json(StatusCode::OK, &response)
}

async fn crawl(
    State(runtime): State<RuntimeState>,
    payload: Result<Json<CrawlRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let request = extract_json(payload)?.0;
    if !(1..=MAX_CRAWL_PAGES).contains(&request.max_pages)
        || !(1..=MAX_CRAWL_CONCURRENCY).contains(&request.concurrency)
    {
        return Err(ApiError::invalid_request());
    }
    let start_url_text = request.start_url.ok_or_else(ApiError::invalid_request)?;
    let start_url = Url::parse(&start_url_text).map_err(|_| ApiError::invalid_request())?;
    let preflight = FetchRequest::request(&start_url_text).map_err(ApiError::from_core)?;
    runtime
        .api
        .context
        .fetch
        .preflight_request(&preflight)
        .map_err(ApiError::from_core)?;
    let config = CrawlConfig {
        start_url,
        max_pages: request.max_pages,
        concurrency: request.concurrency,
        same_origin_only: true,
        include_subdomains: false,
        respect_robots: true,
        minimum_delay: Duration::ZERO,
        proxies: Vec::new(),
    };
    let _permit = acquire_operation(&runtime.api)?;
    let (mut stream, _control, _stats) = Crawler::new(runtime.api.context.fetch.clone())
        .stream(config)
        .map_err(ApiError::from_core)?;
    let mut pages = Vec::with_capacity(request.max_pages);
    let mut remaining_markdown_bytes = MAX_RESPONSE_BYTES;
    while let Some(result) = stream.next().await {
        let page = result.map_err(ApiError::from_core)?;
        let markdown = render_crawl_markdown(
            &page,
            runtime.api.context.fetch.limits().max_output_chars,
            remaining_markdown_bytes,
        )?;
        remaining_markdown_bytes -= markdown.len();
        pages.push(CrawlPageResponse {
            url: page.url,
            status: page.status,
            markdown,
        });
    }
    let response = CrawlResponse {
        start_url: start_url_text,
        count: pages.len(),
        pages,
    };
    bounded_json(StatusCode::OK, &response)
}

fn render_crawl_markdown(
    page: &rscraper_core::CrawlResult,
    per_page_max_chars: usize,
    remaining_markdown_bytes: usize,
) -> Result<String, ApiError> {
    if !(200..300).contains(&page.status) {
        return Err(ApiError::from_core(Error::HttpStatus {
            status: page.status,
            url: page.url.clone(),
        }));
    }

    let aggregate_budget_is_tighter = remaining_markdown_bytes <= per_page_max_chars;
    let render_max_chars = per_page_max_chars.min(remaining_markdown_bytes.saturating_add(1));
    let markdown = match html_to_markdown_with_options(
        &page.html,
        &MarkdownOptions {
            base_url: Some(page.url.clone()),
            max_chars: render_max_chars,
        },
    ) {
        Ok(markdown) => markdown,
        Err(Error::BodyLimit { .. }) if aggregate_budget_is_tighter => {
            return Err(ApiError::response_too_large());
        }
        Err(error) => return Err(ApiError::from_core(error)),
    };
    if markdown.len() > remaining_markdown_bytes {
        return Err(ApiError::response_too_large());
    }
    Ok(markdown)
}

fn acquire_operation(state: &ApiState) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    state
        .operation_limit
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::server_busy())
}

async fn authenticate_operation_path(
    State(runtime): State<RuntimeState>,
    request: Request,
    next: Next,
) -> Response {
    if !matches!(
        request.uri().path(),
        "/scrape" | "/scrape/" | "/search" | "/search/" | "/crawl" | "/crawl/"
    ) {
        return next.run(request).await;
    }
    let Some(expected) = runtime.api.token.as_deref() else {
        return next.run(request).await;
    };
    let mut values = request.headers().get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return ApiError::unauthorized().into_response();
    };
    if values.next().is_some() {
        return ApiError::unauthorized().into_response();
    }
    let presented = value.as_bytes();
    if presented.len() < 7
        || !presented[..6].eq_ignore_ascii_case(b"Bearer")
        || presented[6] != b' '
    {
        return ApiError::unauthorized().into_response();
    }
    let presented = &presented[7..];
    let expected = expected.as_bytes();
    if presented.len() != expected.len() || !bool::from(presented.ct_eq(expected)) {
        return ApiError::unauthorized().into_response();
    }
    next.run(request).await
}

async fn thirty_second_deadline(request: Request, next: Next) -> Response {
    run_with_deadline(SCRAPE_SEARCH_DEADLINE, request, next).await
}

async fn crawl_deadline(request: Request, next: Next) -> Response {
    run_with_deadline(CRAWL_DEADLINE, request, next).await
}

async fn run_with_deadline(duration: Duration, request: Request, next: Next) -> Response {
    match tokio::time::timeout(duration, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::route_timeout().into_response(),
    }
}

fn panic_response(_panic: Box<dyn Any + Send + 'static>) -> Response<Body> {
    tracing::error!("request handler panicked");
    ApiError::internal().into_response()
}

async fn not_found() -> Response {
    ApiError::not_found().into_response()
}

async fn method_not_allowed() -> Response {
    ApiError::method_not_allowed().into_response()
}

fn extract_json<T>(payload: Result<Json<T>, JsonRejection>) -> Result<Json<T>, ApiError> {
    payload.map_err(|rejection| match rejection {
        JsonRejection::MissingJsonContentType(_) => ApiError::unsupported_media_type(),
        JsonRejection::JsonSyntaxError(_) => ApiError::invalid_json(),
        JsonRejection::JsonDataError(_) => ApiError::invalid_json(),
        JsonRejection::BytesRejection(rejection)
            if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE =>
        {
            ApiError::request_too_large()
        }
        JsonRejection::BytesRejection(_) => ApiError::invalid_json(),
        _ => ApiError::invalid_json(),
    })
}

fn bounded_json<T: Serialize>(status: StatusCode, value: &T) -> Result<Response, ApiError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ApiError::internal())?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ApiError::response_too_large());
    }
    Ok(json_bytes_response(status, bytes))
}

fn json_bytes_response(status: StatusCode, bytes: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScrapeRequest {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchRequest {
    #[serde(default)]
    query: Option<String>,
    #[serde(default = "default_search_results")]
    n: usize,
    #[serde(default)]
    scrape: bool,
}

fn default_search_results() -> usize {
    DEFAULT_SEARCH_RESULTS
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CrawlRequest {
    #[serde(default, alias = "url")]
    start_url: Option<String>,
    #[serde(default = "default_crawl_pages")]
    max_pages: usize,
    #[serde(default = "default_crawl_concurrency")]
    concurrency: usize,
}

fn default_crawl_pages() -> usize {
    DEFAULT_CRAWL_PAGES
}

fn default_crawl_concurrency() -> usize {
    DEFAULT_CRAWL_CONCURRENCY
}

#[derive(Serialize)]
struct CrawlPageResponse {
    url: Url,
    status: u16,
    markdown: String,
}

#[derive(Serialize)]
struct CrawlResponse {
    start_url: String,
    count: usize,
    pages: Vec<CrawlPageResponse>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
    code: &'static str,
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    fn from_core(error: Error) -> Self {
        match error {
            Error::InvalidInput(_) | Error::Policy(_) => Self::invalid_request(),
            Error::Timeout { .. } => Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "the upstream operation timed out",
            ),
            Error::RobotsDenied(_) => Self::new(
                StatusCode::FORBIDDEN,
                "request_denied",
                "the request was denied by network policy",
            ),
            Error::Cancelled => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "operation_cancelled",
                "the operation was cancelled",
            ),
            Error::Dns(_)
            | Error::BodyLimit { .. }
            | Error::HttpStatus { .. }
            | Error::Browser(_)
            | Error::Parse { .. }
            | Error::Authentication(_)
            | Error::RateLimited { .. }
            | Error::UpstreamLayout { .. }
            | Error::Io(_)
            | Error::Http(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "the upstream operation failed",
            ),
        }
    }

    fn invalid_request() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the request is invalid",
        )
    }

    fn unsupported_media_type() -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "content type must be application/json",
        )
    }

    fn invalid_json() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "the JSON request body is malformed",
        )
    }

    fn request_too_large() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "the request body exceeds 64 KiB",
        )
    }

    fn response_too_large() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "response_too_large",
            "the operation result exceeds the response limit",
        )
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid bearer authentication is required",
        )
    }

    fn server_busy() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_busy",
            "the server operation limit is exhausted",
        )
    }

    fn route_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "route_timeout",
            "the route deadline was exceeded",
        )
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "route not found")
    }

    fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method not allowed",
        )
    }

    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "the server could not complete the request",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            error: self.message,
            code: self.code,
        };
        match serde_json::to_vec(&body) {
            Ok(bytes) => json_bytes_response(self.status, bytes),
            Err(_) => json_bytes_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                br#"{"error":"the server could not complete the request","code":"internal_error"}"#
                    .to_vec(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn crawl_result(status: u16, html: &str) -> rscraper_core::CrawlResult {
        rscraper_core::CrawlResult {
            url: Url::parse("https://example.com/page").unwrap(),
            status,
            html: html.to_owned(),
            links: Vec::new(),
        }
    }

    #[test]
    fn crawl_markdown_distinguishes_exact_aggregate_and_per_page_boundaries() {
        let exact = match render_crawl_markdown(&crawl_result(200, "xxxxx"), 100, 5) {
            Ok(markdown) => markdown,
            Err(_) => panic!("exact aggregate boundary was rejected"),
        };
        assert_eq!(exact, "xxxxx");

        let aggregate = render_crawl_markdown(&crawl_result(200, "xxxxxx"), 100, 5).unwrap_err();
        assert_eq!(aggregate.status, StatusCode::BAD_GATEWAY);
        assert_eq!(aggregate.code, "response_too_large");

        let per_page = render_crawl_markdown(&crawl_result(200, "xxxxxx"), 5, 100).unwrap_err();
        assert_eq!(per_page.status, StatusCode::BAD_GATEWAY);
        assert_eq!(per_page.code, "upstream_error");
    }

    #[tokio::test]
    async fn panic_middleware_returns_the_stable_json_error_without_the_panic_payload() {
        async fn panics() -> &'static str {
            panic!("private-panic-payload")
        }

        install_redacted_panic_hook();
        let app = Router::new()
            .route("/panic", get(panics))
            .layer(CatchPanicLayer::custom(panic_response));
        let response = app
            .oneshot(HttpRequest::get("/panic").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "error":"the server could not complete the request",
                "code":"internal_error"
            })
        );
        assert!(!String::from_utf8_lossy(&body).contains("private-panic-payload"));
    }
}
