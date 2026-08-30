use axum::body::{to_bytes, Body};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, Request, StatusCode};
use rscraper_api::{
    router, router_with_search_endpoints, serve_with_shutdown, validate_server_config, ApiState,
    ServerConfig,
};
use rscraper_cli::context::AppContext;
use rscraper_cli::web::SearchEndpoints;
use rscraper_core::{FetchClient, NetworkPolicy, OperationLimits};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tower::ServiceExt;
use url::Url;

const JSON: &str = "application/json";

fn public_context() -> AppContext {
    AppContext {
        fetch: FetchClient::builder().build().unwrap(),
        browser: None,
        config_dir: PathBuf::new(),
    }
}

fn local_context() -> AppContext {
    context_with_limits(OperationLimits::default())
}

fn context_with_limits(limits: OperationLimits) -> AppContext {
    AppContext {
        fetch: FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .limits(limits)
            .build()
            .unwrap(),
        browser: None,
        config_dir: PathBuf::new(),
    }
}

fn state(context: AppContext, token: Option<&str>, permits: usize) -> ApiState {
    ApiState {
        context,
        token: token.map(Arc::<str>::from),
        operation_limit: Arc::new(Semaphore::new(permits)),
    }
}

fn json_request(method: Method, uri: &str, value: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(CONTENT_TYPE, JSON)
        .body(Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap()
}

async fn response_json(response: axum::response::Response) -> Value {
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let bytes = to_bytes(response.into_body(), 12 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn assert_error(response: axum::response::Response, status: StatusCode, code: &str) -> Value {
    assert_eq!(response.status(), status);
    let value = response_json(response).await;
    assert_eq!(value["code"], code);
    assert!(value["error"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
    value
}

#[test]
fn startup_rejects_every_non_loopback_bind_without_a_token() {
    for bind in [
        "0.0.0.0:8787",
        "[::]:8787",
        "10.1.2.3:8787",
        "192.168.1.2:8787",
        "169.254.10.2:8787",
        "224.0.0.1:8787",
        "[fe80::1]:8787",
        "[ff02::1]:8787",
        "8.8.8.8:8787",
        "[2606:4700:4700::1111]:8787",
    ] {
        let config = ServerConfig {
            bind: bind.parse().unwrap(),
            token: None,
            max_concurrent_operations: 8,
        };
        assert!(
            validate_server_config(&config).is_err(),
            "accepted unauthenticated bind {bind}"
        );
    }
}

#[test]
fn startup_accepts_loopback_without_a_token_and_non_loopback_with_one() {
    for bind in ["127.0.0.1:8787", "[::1]:8787"] {
        validate_server_config(&ServerConfig {
            bind: bind.parse().unwrap(),
            token: None,
            max_concurrent_operations: 8,
        })
        .unwrap();
    }

    validate_server_config(&ServerConfig {
        bind: "0.0.0.0:8787".parse().unwrap(),
        token: Some("server-owned-secret".into()),
        max_concurrent_operations: 8,
    })
    .unwrap();
}

#[test]
fn startup_rejects_empty_tokens_and_operation_limits_outside_one_through_32() {
    for token in ["", "   ", "contains space", "line\nbreak", "unicode-🦀"] {
        let config = ServerConfig {
            bind: "127.0.0.1:8787".parse().unwrap(),
            token: Some(token.into()),
            max_concurrent_operations: 8,
        };
        assert!(validate_server_config(&config).is_err());
    }
    for limit in [0, 33] {
        let config = ServerConfig {
            bind: "127.0.0.1:8787".parse().unwrap(),
            token: None,
            max_concurrent_operations: limit,
        };
        assert!(validate_server_config(&config).is_err());
    }
}

#[test]
fn startup_parsing_has_safe_defaults_and_fails_explicitly_on_malformed_values() {
    let defaults = ServerConfig::from_lookup(|_| None).unwrap();
    assert_eq!(
        defaults.bind,
        "127.0.0.1:8787".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(defaults.max_concurrent_operations, 8);
    assert!(defaults.token.is_none());

    for (name, value) in [
        ("PORT", "not-a-port"),
        ("PORT", "70000"),
        ("RSCRAPER_BIND", "not-an-address"),
        ("RSCRAPER_API_MAX_CONCURRENT_OPERATIONS", "many"),
        ("RSCRAPER_API_MAX_CONCURRENT_OPERATIONS", "0"),
    ] {
        let values = HashMap::from([(name.to_string(), value.to_string())]);
        assert!(
            ServerConfig::from_lookup(|key| values.get(key).cloned()).is_err(),
            "accepted {name}={value}"
        );
    }

    let values = HashMap::from([
        ("PORT".to_string(), "9999".to_string()),
        ("RSCRAPER_BIND".to_string(), "127.0.0.1:4321".to_string()),
        (
            "RSCRAPER_API_TOKEN".to_string(),
            "lookup-owned-secret".to_string(),
        ),
        (
            "RSCRAPER_API_MAX_CONCURRENT_OPERATIONS".to_string(),
            "12".to_string(),
        ),
    ]);
    let parsed = ServerConfig::from_lookup(|key| values.get(key).cloned()).unwrap();
    assert_eq!(parsed.bind, "127.0.0.1:4321".parse().unwrap());
    assert_eq!(parsed.token.as_deref(), Some("lookup-owned-secret"));
    assert_eq!(parsed.max_concurrent_operations, 12);
}

#[test]
fn startup_binary_fails_before_binding_without_writing_secrets_or_logs_to_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_rscraper-api"))
        .env("RSCRAPER_BIND", "0.0.0.0:8787")
        .env("RSCRAPER_API_TOKEN", "startup-cli-secret")
        .env("RSCRAPER_API_MAX_CONCURRENT_OPERATIONS", "not-a-number")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("startup-cli-secret"));
}

#[cfg(unix)]
#[test]
fn startup_rejects_non_unicode_configuration_instead_of_silently_defaulting() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    for name in [
        "PORT",
        "RSCRAPER_BIND",
        "RSCRAPER_API_TOKEN",
        "RSCRAPER_API_MAX_CONCURRENT_OPERATIONS",
    ] {
        let invalid = OsString::from_vec(vec![0xff]);
        assert!(
            ServerConfig::from_os_lookup(|key| (key == name).then(|| invalid.clone())).is_err(),
            "accepted non-Unicode {name}"
        );
    }
}

#[tokio::test]
async fn health_is_unauthenticated_json_and_does_no_remote_work() {
    let app = router(state(public_context(), Some("correct-token"), 1));
    let response = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    assert!(response.headers().get("x-request-id").is_some());
    assert_eq!(
        response_json(response).await,
        json!({"status":"ok", "service":"rscraper-api"})
    );
}

#[tokio::test]
async fn bearer_auth_rejects_missing_malformed_duplicate_non_utf8_and_wrong_tokens() {
    let cases: Vec<Vec<HeaderValue>> = vec![
        vec![],
        vec![HeaderValue::from_static("Basic abc")],
        vec![HeaderValue::from_static("Bearer")],
        vec![HeaderValue::from_static("Bearer short")],
        vec![HeaderValue::from_static("Bearer wrong-token!")],
        vec![
            HeaderValue::from_static("Bearer correct-token"),
            HeaderValue::from_static("Bearer correct-token"),
        ],
        vec![HeaderValue::from_bytes(b"Bearer \xff\xfe").unwrap()],
    ];

    for values in cases {
        let mut request = json_request(Method::POST, "/scrape", json!({"url":"bad"}));
        for value in values {
            request.headers_mut().append(AUTHORIZATION, value);
        }
        let response = router(state(public_context(), Some("correct-token"), 1))
            .oneshot(request)
            .await
            .unwrap();
        let body = assert_error(response, StatusCode::UNAUTHORIZED, "unauthorized").await;
        let text = body.to_string();
        assert!(!text.contains("correct-token"));
        assert!(!text.contains("wrong-token"));
    }
}

#[tokio::test]
async fn bearer_auth_accepts_the_exact_token_and_debug_output_is_redacted() {
    let api_state = state(public_context(), Some("correct-token"), 1);
    let config = ServerConfig {
        bind: "127.0.0.1:8787".parse().unwrap(),
        token: Some("correct-token".into()),
        max_concurrent_operations: 1,
    };
    assert!(!format!("{api_state:?}").contains("correct-token"));
    assert!(!format!("{config:?}").contains("correct-token"));

    let mut request = json_request(Method::POST, "/scrape", json!({"url":"bad"}));
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer correct-token"),
    );
    let response = router(api_state).oneshot(request).await.unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_operation_paths_authenticate_before_returning_method_errors() {
    let request = Request::builder()
        .method(Method::PUT)
        .uri("/scrape")
        .body(Body::empty())
        .unwrap();
    let response = router(state(public_context(), Some("correct-token"), 1))
        .oneshot(request)
        .await
        .unwrap();
    assert_error(response, StatusCode::UNAUTHORIZED, "unauthorized").await;

    let mut request = Request::builder()
        .method(Method::PUT)
        .uri("/scrape")
        .body(Body::empty())
        .unwrap();
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer correct-token"),
    );
    let response = router(state(public_context(), Some("correct-token"), 1))
        .oneshot(request)
        .await
        .unwrap();
    assert_error(
        response,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    )
    .await;
}

#[tokio::test]
async fn json_extraction_rejects_content_type_syntax_unknown_fields_and_oversized_bodies() {
    let no_content_type = Request::post("/scrape")
        .body(Body::from(r#"{"url":"https://example.com"}"#))
        .unwrap();
    assert_error(
        router(state(public_context(), None, 1))
            .oneshot(no_content_type)
            .await
            .unwrap(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    )
    .await;

    let wrong_content_type = Request::post("/scrape")
        .header(CONTENT_TYPE, "text/plain")
        .body(Body::from(r#"{"url":"https://example.com"}"#))
        .unwrap();
    assert_error(
        router(state(public_context(), None, 1))
            .oneshot(wrong_content_type)
            .await
            .unwrap(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    )
    .await;

    let malformed = Request::post("/scrape")
        .header(CONTENT_TYPE, JSON)
        .body(Body::from("{"))
        .unwrap();
    assert_error(
        router(state(public_context(), None, 1))
            .oneshot(malformed)
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_json",
    )
    .await;

    let unknown = json_request(
        Method::POST,
        "/scrape",
        json!({"url":"https://example.com", "private_network":true}),
    );
    assert_error(
        router(state(public_context(), None, 1))
            .oneshot(unknown)
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_json",
    )
    .await;

    let large = format!(r#"{{"url":"https://example.com/{}"}}"#, "a".repeat(65_537));
    let oversized = Request::post("/scrape")
        .header(CONTENT_TYPE, JSON)
        .body(Body::from(large))
        .unwrap();
    assert_error(
        router(state(public_context(), None, 1))
            .oneshot(oversized)
            .await
            .unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "request_too_large",
    )
    .await;
}

#[tokio::test]
async fn scrape_rejects_missing_invalid_unsafe_and_credential_bearing_urls_without_leaks() {
    for (value, forbidden) in [
        (json!({}), None),
        (json!({"url":"not a url"}), None),
        (json!({"url":"http://127.0.0.1/private"}), None),
        (
            json!({"url":"https://user:credential-secret@example.com/"}),
            Some("credential-secret"),
        ),
    ] {
        let response = router(state(public_context(), None, 1))
            .oneshot(json_request(Method::POST, "/scrape", value))
            .await
            .unwrap();
        let body = assert_error(response, StatusCode::BAD_REQUEST, "invalid_request").await;
        if let Some(secret) = forbidden {
            assert!(!body.to_string().contains(secret));
        }
    }
}

#[tokio::test]
async fn search_and_crawl_enforce_every_numeric_bound_before_network_work() {
    for n in [0, 21] {
        assert_error(
            router(state(public_context(), None, 1))
                .oneshot(json_request(
                    Method::POST,
                    "/search",
                    json!({"query":"rust", "n":n}),
                ))
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        )
        .await;
    }

    for (field, value) in [
        ("max_pages", 0),
        ("max_pages", 101),
        ("concurrency", 0),
        ("concurrency", 17),
    ] {
        let mut payload = serde_json::Map::from_iter([(
            "start_url".into(),
            Value::String("https://example.com/".into()),
        )]);
        payload.insert(field.into(), Value::from(value));
        assert_error(
            router(state(public_context(), None, 1))
                .oneshot(json_request(Method::POST, "/crawl", Value::Object(payload)))
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        )
        .await;
    }
}

#[tokio::test]
async fn search_defaults_to_five_and_accepts_the_exact_maximum_of_twenty() {
    let server = TestServer::spawn(|target| {
        if target.starts_with("/ddg") {
            let mut html = String::new();
            for index in 0..25 {
                html.push_str(&format!(
                    "<div class='result'><a class='result__a' href='https://example.com/{index}'>Hit {index}</a><div class='result__snippet'>Snippet {index}</div></div>"
                ));
            }
            TestResponse::html(200, html)
        } else {
            TestResponse::html(404, "missing")
        }
    })
    .await;
    let endpoints = SearchEndpoints {
        duckduckgo: server.url_value("/ddg"),
        bing: server.url_value("/bing"),
    };

    let default_response =
        router_with_search_endpoints(state(local_context(), None, 1), endpoints.clone())
            .oneshot(json_request(
                Method::POST,
                "/search",
                json!({"query":"rust"}),
            ))
            .await
            .unwrap();
    assert_eq!(response_json(default_response).await["count"], 5);

    let maximum_response = router_with_search_endpoints(state(local_context(), None, 1), endpoints)
        .oneshot(json_request(
            Method::POST,
            "/search",
            json!({"query":"rust", "n":20}),
        ))
        .await
        .unwrap();
    assert_eq!(response_json(maximum_response).await["count"], 20);
}

#[tokio::test]
async fn crawl_defaults_to_twenty_pages_and_four_workers_and_accepts_exact_maxima() {
    let chain = TestServer::spawn(|target| {
        if target == "/robots.txt" {
            return TestResponse::text(404, "missing");
        }
        let index = target
            .trim_start_matches("/page/")
            .parse::<usize>()
            .unwrap_or_default();
        TestResponse::html(
            200,
            format!(
                "<main><p>Page {index}</p><a href='/page/{}'>next</a></main>",
                index + 1
            ),
        )
    })
    .await;
    let response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":chain.url("/page/0")}),
        ))
        .await
        .unwrap();
    assert_eq!(response_json(response).await["count"], 20);

    let parallel = TestServer::spawn(|target| match target {
        "/robots.txt" => TestResponse::text(404, "missing"),
        "/" => TestResponse::html(
            200,
            (0..8)
                .map(|index| format!("<a href='/leaf/{index}'>leaf</a>"))
                .collect::<String>(),
        ),
        _ if target.starts_with("/leaf/") => {
            TestResponse::html(200, "<main>leaf</main>").delayed(Duration::from_millis(100))
        }
        _ => TestResponse::html(404, "missing"),
    })
    .await;
    let response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":parallel.url("/")}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(parallel.max_in_flight(), 4);

    let maximum = TestServer::spawn(|target| match target {
        "/robots.txt" => TestResponse::text(404, "missing"),
        _ => TestResponse::html(200, "<main>single</main>"),
    })
    .await;
    let response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({
                "start_url":maximum.url("/"),
                "max_pages":100,
                "concurrency":16
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn search_rejects_missing_empty_and_oversized_queries_before_network_work() {
    let server = TestServer::spawn(|_| TestResponse::html(500, "must-not-run")).await;
    let endpoints = SearchEndpoints {
        duckduckgo: server.url_value("/ddg"),
        bing: server.url_value("/bing"),
    };
    for payload in [
        json!({}),
        json!({"query":"   "}),
        json!({"query":"x".repeat(1_025)}),
    ] {
        let response =
            router_with_search_endpoints(state(local_context(), None, 1), endpoints.clone())
                .oneshot(json_request(Method::POST, "/search", payload))
                .await
                .unwrap();
        assert_error(response, StatusCode::BAD_REQUEST, "invalid_request").await;
    }
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn oversized_search_query_is_rejected_before_operation_permit_acquisition() {
    let response = router(state(public_context(), None, 0))
        .oneshot(json_request(
            Method::POST,
            "/search",
            json!({"query":"q".repeat(1_025)}),
        ))
        .await
        .unwrap();

    assert_error(response, StatusCode::BAD_REQUEST, "invalid_request").await;
}

#[tokio::test]
async fn crawl_rejects_missing_unsafe_and_credential_bearing_start_urls() {
    for payload in [
        json!({}),
        json!({"start_url":"http://127.0.0.1/private"}),
        json!({"start_url":"https://user:secret@example.com/"}),
    ] {
        let response = router(state(public_context(), None, 1))
            .oneshot(json_request(Method::POST, "/crawl", payload))
            .await
            .unwrap();
        let body = assert_error(response, StatusCode::BAD_REQUEST, "invalid_request").await;
        assert!(!body.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn scrape_success_preserves_the_documented_response_shape() {
    let server = TestServer::spawn(|target| {
        assert_eq!(target, "/article");
        TestResponse::html(200, "<main><h1>Fixture</h1><p>Local body.</p></main>")
    })
    .await;
    let response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/scrape",
            json!({"url":server.url("/article")}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["url"], server.url("/article"));
    assert_eq!(body["status"], 200);
    assert_eq!(body["markdown"], "# Fixture\n\nLocal body.");
}

#[tokio::test]
async fn search_success_uses_the_typed_service_and_preserves_response_fields() {
    let server = TestServer::spawn(|target| {
        if target.starts_with("/ddg") {
            TestResponse::html(
                200,
                r#"<div class="result"><a class="result__a" href="https://example.com/a">Hit A</a><div class="result__snippet">Snippet A</div></div>"#,
            )
        } else {
            TestResponse::html(404, "missing")
        }
    })
    .await;
    let app = router_with_search_endpoints(
        state(local_context(), None, 1),
        SearchEndpoints {
            duckduckgo: server.url_value("/ddg"),
            bing: server.url_value("/bing"),
        },
    );
    let response = app
        .oneshot(json_request(
            Method::POST,
            "/search",
            json!({"query":"rust", "n":1}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["query"], "rust");
    assert_eq!(body["count"], 1);
    assert_eq!(body["results"][0]["title"], "Hit A");
    assert_eq!(body["provider"], "duckduckgo");
}

#[tokio::test]
async fn crawl_success_uses_the_central_crawler_and_preserves_response_fields() {
    let server = TestServer::spawn(|target| match target {
        "/robots.txt" => TestResponse::text(404, "missing"),
        "/" => TestResponse::html(200, "<main><p>First</p><a href='/second'>next</a></main>"),
        "/second" => TestResponse::html(200, "<main><p>Second</p></main>"),
        _ => TestResponse::html(404, "missing"),
    })
    .await;
    let response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":server.url("/"), "max_pages":2, "concurrency":2}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["start_url"], server.url("/"));
    assert_eq!(body["count"], 2);
    assert_eq!(body["pages"].as_array().unwrap().len(), 2);
    assert!(body["pages"].as_array().unwrap().iter().all(|page| {
        page.get("url").is_some() && page.get("status").is_some() && page.get("markdown").is_some()
    }));
}

#[tokio::test]
async fn crawl_non_success_pages_are_generic_gateway_errors_without_body_leaks() {
    for status in [404, 429, 500] {
        let sentinel = format!("private-crawl-{status}-body-sentinel");
        let response_body = sentinel.clone();
        let server = TestServer::spawn(move |target| match target {
            "/robots.txt" => TestResponse::text(404, "missing"),
            "/failure" => TestResponse::html(status, response_body.clone()),
            _ => TestResponse::html(404, "missing"),
        })
        .await;
        let response = router(state(local_context(), None, 1))
            .oneshot(json_request(
                Method::POST,
                "/crawl",
                json!({"start_url":server.url("/failure"), "max_pages":1}),
            ))
            .await
            .unwrap();

        let body = assert_error(response, StatusCode::BAD_GATEWAY, "upstream_error").await;
        assert!(!body.to_string().contains(&sentinel));
    }
}

#[tokio::test]
async fn crawl_preserves_typed_robots_denial_and_upstream_failure_mappings() {
    let denied = TestServer::spawn(|target| match target {
        "/robots.txt" => TestResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /blocked-url-secret-sentinel\n",
        ),
        "/blocked-url-secret-sentinel" => {
            TestResponse::html(200, "blocked-page-body-secret-sentinel")
        }
        _ => TestResponse::html(404, "missing"),
    })
    .await;
    let denied_response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":denied.url("/blocked-url-secret-sentinel")}),
        ))
        .await
        .unwrap();
    let denied_body = assert_error(denied_response, StatusCode::FORBIDDEN, "request_denied").await;
    let denied_text = denied_body.to_string();
    assert!(!denied_text.contains("blocked-url-secret-sentinel"));
    assert!(!denied_text.contains("blocked-page-body-secret-sentinel"));
    assert_eq!(denied.hits(), 1);

    let failed = TestServer::spawn(|target| match target {
        "/robots.txt" => TestResponse::text(500, "robots-body-secret-sentinel"),
        "/" => TestResponse::html(200, "page-body-secret-sentinel"),
        _ => TestResponse::html(404, "missing"),
    })
    .await;
    let failed_response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":failed.url("/")}),
        ))
        .await
        .unwrap();
    let failed_body =
        assert_error(failed_response, StatusCode::BAD_GATEWAY, "upstream_error").await;
    let failed_text = failed_body.to_string();
    assert!(!failed_text.contains("robots-body-secret-sentinel"));
    assert!(!failed_text.contains("page-body-secret-sentinel"));
    assert_eq!(failed.hits(), 1);
}

#[tokio::test]
async fn initial_multi_hop_redirect_to_robots_denial_is_a_redacted_forbidden_response() {
    let blocked_hits = Arc::new(AtomicUsize::new(0));
    let fixture_blocked_hits = blocked_hits.clone();
    let server = TestServer::spawn(move |target| match target {
        "/robots.txt" => TestResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /blocked-url-secret-sentinel\n",
        ),
        "/start" => TestResponse::redirect("/middle"),
        "/middle" => TestResponse::redirect("/blocked-url-secret-sentinel"),
        "/blocked-url-secret-sentinel" => {
            fixture_blocked_hits.fetch_add(1, Ordering::SeqCst);
            TestResponse::html(200, "blocked-page-body-secret-sentinel")
        }
        _ => TestResponse::html(404, "missing"),
    })
    .await;
    let response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":server.url("/start"), "max_pages":1}),
        ))
        .await
        .unwrap();

    let body = assert_error(response, StatusCode::FORBIDDEN, "request_denied").await;
    let body = body.to_string();
    assert!(!body.contains("blocked-url-secret-sentinel"));
    assert!(!body.contains("blocked-page-body-secret-sentinel"));
    assert_eq!(blocked_hits.load(Ordering::SeqCst), 0);
    assert_eq!(server.hits(), 3);
}

#[tokio::test]
async fn later_discovered_redirect_to_robots_denial_is_a_silent_api_skip() {
    let blocked_hits = Arc::new(AtomicUsize::new(0));
    let fixture_blocked_hits = blocked_hits.clone();
    let server = TestServer::spawn(move |target| match target {
        "/robots.txt" => TestResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /blocked-url-secret-sentinel\n",
        ),
        "/" => TestResponse::html(
            200,
            "<a href='/alias'>alias</a><a href='/allowed'>allowed</a>",
        ),
        "/alias" => TestResponse::redirect("/blocked-url-secret-sentinel"),
        "/allowed" => TestResponse::html(200, "allowed page"),
        "/blocked-url-secret-sentinel" => {
            fixture_blocked_hits.fetch_add(1, Ordering::SeqCst);
            TestResponse::html(200, "blocked-page-body-secret-sentinel")
        }
        _ => TestResponse::html(404, "missing"),
    })
    .await;
    let response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":server.url("/"), "max_pages":3, "concurrency":1}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["count"], 2);
    assert_eq!(body["pages"].as_array().unwrap().len(), 2);
    let body = body.to_string();
    assert!(!body.contains("blocked-url-secret-sentinel"));
    assert!(!body.contains("blocked-page-body-secret-sentinel"));
    assert_eq!(blocked_hits.load(Ordering::SeqCst), 0);
    assert_eq!(server.hits(), 4);
}

#[tokio::test]
async fn crawl_aggregate_overflow_is_distinct_from_a_per_page_render_limit() {
    const TEN_MIB: usize = 10 * 1024 * 1024;
    let aggregate_body = vec![b'x'; TEN_MIB + 1];
    let aggregate = TestServer::spawn(move |target| match target {
        "/robots.txt" => TestResponse::text(404, "missing"),
        "/" => TestResponse::html(200, aggregate_body.clone()),
        _ => TestResponse::html(404, "missing"),
    })
    .await;
    let aggregate_limits = OperationLimits {
        max_body_bytes: TEN_MIB + 1024,
        max_output_chars: TEN_MIB + 1024,
        ..OperationLimits::default()
    };
    let aggregate_response = router(state(context_with_limits(aggregate_limits), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":aggregate.url("/"), "max_pages":1}),
        ))
        .await
        .unwrap();
    assert_error(
        aggregate_response,
        StatusCode::BAD_GATEWAY,
        "response_too_large",
    )
    .await;

    let per_page = TestServer::spawn(|target| match target {
        "/robots.txt" => TestResponse::text(404, "missing"),
        "/" => TestResponse::html(200, "x".repeat(128)),
        _ => TestResponse::html(404, "missing"),
    })
    .await;
    let per_page_limits = OperationLimits {
        max_body_bytes: 1024,
        max_output_chars: 64,
        ..OperationLimits::default()
    };
    let per_page_response = router(state(context_with_limits(per_page_limits), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":per_page.url("/"), "max_pages":1}),
        ))
        .await
        .unwrap();
    assert_error(per_page_response, StatusCode::BAD_GATEWAY, "upstream_error").await;
}

#[tokio::test]
async fn upstream_status_timeout_and_body_limit_have_stable_gateway_mappings() {
    let not_found = TestServer::spawn(|_| TestResponse::html(404, "private-upstream-body")).await;
    let response = router(state(local_context(), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/scrape",
            json!({"url":not_found.url("/missing")}),
        ))
        .await
        .unwrap();
    let body = assert_error(response, StatusCode::BAD_GATEWAY, "upstream_error").await;
    assert!(!body.to_string().contains("private-upstream-body"));

    let slow = TestServer::spawn(|_| {
        TestResponse::html(200, "<main>late</main>").delayed(Duration::from_millis(250))
    })
    .await;
    let timeout_limits = OperationLimits {
        connect_timeout: Duration::from_millis(50),
        request_timeout: Duration::from_millis(75),
        ..OperationLimits::default()
    };
    assert_error(
        router(state(context_with_limits(timeout_limits), None, 1))
            .oneshot(json_request(
                Method::POST,
                "/scrape",
                json!({"url":slow.url("/slow")}),
            ))
            .await
            .unwrap(),
        StatusCode::GATEWAY_TIMEOUT,
        "upstream_timeout",
    )
    .await;

    let large = TestServer::spawn(|_| TestResponse::html(200, "x".repeat(256))).await;
    let body_limits = OperationLimits {
        max_body_bytes: 64,
        ..OperationLimits::default()
    };
    assert_error(
        router(state(context_with_limits(body_limits), None, 1))
            .oneshot(json_request(
                Method::POST,
                "/scrape",
                json!({"url":large.url("/large")}),
            ))
            .await
            .unwrap(),
        StatusCode::BAD_GATEWAY,
        "upstream_error",
    )
    .await;
}

#[tokio::test]
async fn exhausted_operation_limit_fails_immediately_and_releases_after_success_and_error() {
    let semaphore = Arc::new(Semaphore::new(1));
    let held = semaphore.clone().acquire_owned().await.unwrap();
    let api_state = ApiState {
        context: public_context(),
        token: None,
        operation_limit: semaphore.clone(),
    };
    let response = tokio::time::timeout(
        Duration::from_millis(100),
        router(api_state.clone()).oneshot(json_request(
            Method::POST,
            "/scrape",
            json!({"url":"https://example.com/"}),
        )),
    )
    .await
    .expect("semaphore exhaustion queued instead of failing")
    .unwrap();
    assert_error(response, StatusCode::SERVICE_UNAVAILABLE, "server_busy").await;
    drop(held);

    let server = TestServer::spawn(|_| TestResponse::html(200, "<main>ok</main>")).await;
    let success = router(ApiState {
        context: local_context(),
        token: None,
        operation_limit: semaphore.clone(),
    })
    .oneshot(json_request(
        Method::POST,
        "/scrape",
        json!({"url":server.url("/")}),
    ))
    .await
    .unwrap();
    assert_eq!(success.status(), StatusCode::OK);
    assert_eq!(semaphore.available_permits(), 1);

    let error = router(ApiState {
        context: public_context(),
        token: None,
        operation_limit: semaphore.clone(),
    })
    .oneshot(json_request(Method::POST, "/scrape", json!({"url":"bad"})))
    .await
    .unwrap();
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test]
async fn search_and_crawl_cannot_start_remote_work_without_an_operation_permit() {
    let server = TestServer::spawn(|_| TestResponse::html(200, "must-not-run")).await;
    let semaphore = Arc::new(Semaphore::new(1));
    let held = semaphore.clone().acquire_owned().await.unwrap();
    let shared_state = ApiState {
        context: local_context(),
        token: None,
        operation_limit: semaphore,
    };

    let search_response = router_with_search_endpoints(
        shared_state.clone(),
        SearchEndpoints {
            duckduckgo: server.url_value("/ddg"),
            bing: server.url_value("/bing"),
        },
    )
    .oneshot(json_request(
        Method::POST,
        "/search",
        json!({"query":"rust"}),
    ))
    .await
    .unwrap();
    assert_error(
        search_response,
        StatusCode::SERVICE_UNAVAILABLE,
        "server_busy",
    )
    .await;

    let crawl_response = router(shared_state)
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":server.url("/")}),
        ))
        .await
        .unwrap();
    assert_error(
        crawl_response,
        StatusCode::SERVICE_UNAVAILABLE,
        "server_busy",
    )
    .await;
    assert_eq!(server.hits(), 0);
    drop(held);
}

#[tokio::test]
async fn upstream_timeout_releases_the_operation_permit() {
    let server = TestServer::spawn(|_| {
        TestResponse::html(200, "<main>late</main>").delayed(Duration::from_millis(250))
    })
    .await;
    let limits = OperationLimits {
        connect_timeout: Duration::from_millis(50),
        request_timeout: Duration::from_millis(75),
        ..OperationLimits::default()
    };
    let semaphore = Arc::new(Semaphore::new(1));
    let response = router(ApiState {
        context: context_with_limits(limits),
        token: None,
        operation_limit: semaphore.clone(),
    })
    .oneshot(json_request(
        Method::POST,
        "/scrape",
        json!({"url":server.url("/slow")}),
    ))
    .await
    .unwrap();
    assert_error(response, StatusCode::GATEWAY_TIMEOUT, "upstream_timeout").await;
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn scrape_and_search_routes_enforce_the_thirty_second_absolute_deadline() {
    let server = TestServer::spawn(|_| {
        TestResponse::html(200, "<main>late</main>").delayed(Duration::from_secs(300))
    })
    .await;
    let limits = OperationLimits {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(120),
        ..OperationLimits::default()
    };
    let task = tokio::spawn(router(state(context_with_limits(limits), None, 1)).oneshot(
        json_request(Method::POST, "/scrape", json!({"url":server.url("/slow")})),
    ));
    while server.hits() == 0 {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(29)).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    let response = task.await.unwrap().unwrap();
    assert_error(response, StatusCode::GATEWAY_TIMEOUT, "route_timeout").await;
}

#[tokio::test(start_paused = true)]
async fn crawl_route_enforces_the_one_hundred_twenty_second_absolute_deadline() {
    let server = TestServer::spawn(|target| {
        if target == "/robots.txt" {
            TestResponse::text(404, "missing")
        } else {
            TestResponse::html(200, "<main>late</main>").delayed(Duration::from_secs(300))
        }
    })
    .await;
    let limits = OperationLimits {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(120),
        ..OperationLimits::default()
    };
    let task = tokio::spawn(router(state(context_with_limits(limits), None, 1)).oneshot(
        json_request(Method::POST, "/crawl", json!({"start_url":server.url("/")})),
    ));
    while server.hits() < 2 {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(119)).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    let response = task.await.unwrap().unwrap();
    assert_error(response, StatusCode::GATEWAY_TIMEOUT, "route_timeout").await;
}

#[tokio::test]
async fn cancelling_remote_work_releases_the_operation_permit() {
    let slow = TestServer::spawn(|_| {
        TestResponse::html(200, "<main>late</main>").delayed(Duration::from_secs(2))
    })
    .await;
    let semaphore = Arc::new(Semaphore::new(1));
    let api_state = ApiState {
        context: local_context(),
        token: None,
        operation_limit: semaphore.clone(),
    };
    let task = tokio::spawn(router(api_state).oneshot(json_request(
        Method::POST,
        "/scrape",
        json!({"url":slow.url("/slow")}),
    )));

    tokio::time::timeout(Duration::from_secs(1), async {
        while slow.hits() == 0 || semaphore.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while semaphore.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled operation retained its permit");
}

#[tokio::test]
async fn cancelling_a_crawl_releases_its_permit_and_cancels_the_crawler_stream() {
    let slow = TestServer::spawn(|target| {
        if target == "/robots.txt" {
            TestResponse::text(404, "missing")
        } else {
            TestResponse::html(200, "<main>late</main>").delayed(Duration::from_secs(2))
        }
    })
    .await;
    let semaphore = Arc::new(Semaphore::new(1));
    let task = tokio::spawn(
        router(ApiState {
            context: local_context(),
            token: None,
            operation_limit: semaphore.clone(),
        })
        .oneshot(json_request(
            Method::POST,
            "/crawl",
            json!({"start_url":slow.url("/")}),
        )),
    );

    tokio::time::timeout(Duration::from_secs(1), async {
        while slow.hits() < 2 || semaphore.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while semaphore.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled crawl retained its permit");
}

#[tokio::test]
async fn serialized_responses_over_ten_mib_are_rejected_before_response_construction() {
    let body = format!("<main><p>{}</p></main>", "x".repeat(10 * 1024 * 1024));
    let server = TestServer::spawn(move |_| TestResponse::html(200, body.clone())).await;
    let limits = OperationLimits {
        max_body_bytes: 11 * 1024 * 1024,
        max_output_chars: 11 * 1024 * 1024,
        ..OperationLimits::default()
    };
    let response = router(state(context_with_limits(limits), None, 1))
        .oneshot(json_request(
            Method::POST,
            "/scrape",
            json!({"url":server.url("/large")}),
        ))
        .await
        .unwrap();
    assert_error(response, StatusCode::BAD_GATEWAY, "response_too_large").await;
}

#[tokio::test]
async fn unknown_routes_and_unsupported_methods_are_stable_json_without_cors() {
    for (request, status, code) in [
        (
            Request::get("/unknown").body(Body::empty()).unwrap(),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            Request::builder()
                .method(Method::PUT)
                .uri("/scrape")
                .body(Body::empty())
                .unwrap(),
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        ),
    ] {
        let response = router(state(public_context(), None, 1))
            .oneshot(request)
            .await
            .unwrap();
        assert!(response
            .headers()
            .get("access-control-allow-origin")
            .is_none());
        assert_error(response, status, code).await;
    }
}

#[tokio::test]
async fn documented_routes_keep_the_existing_single_trailing_slash_compatibility() {
    let health = router(state(public_context(), None, 1))
        .oneshot(Request::get("/health/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    for (path, payload) in [
        ("/scrape/", json!({"url":"bad"})),
        ("/search/", json!({"query":""})),
        ("/crawl/", json!({"start_url":"bad"})),
    ] {
        let response = router(state(public_context(), None, 1))
            .oneshot(json_request(Method::POST, path, payload))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{path} lost trailing-slash compatibility"
        );
    }
}

#[tokio::test]
async fn listener_serves_health_and_gracefully_stops_on_the_supplied_shutdown_future() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve_with_shutdown(
        listener,
        state(public_context(), None, 1),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("\"service\":\"rscraper-api\""));

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("server ignored graceful shutdown")
        .unwrap()
        .unwrap();
}

#[derive(Clone)]
struct TestResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    delay: Duration,
    location: Option<String>,
}

impl TestResponse {
    fn html(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
            delay: Duration::ZERO,
            location: None,
        }
    }

    fn text(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
            delay: Duration::ZERO,
            location: None,
        }
    }

    fn redirect(location: impl Into<String>) -> Self {
        Self {
            status: 302,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
            delay: Duration::ZERO,
            location: Some(location.into()),
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

struct TestServer {
    address: SocketAddr,
    hits: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn spawn<F>(handler: F) -> Self
    where
        F: Fn(&str) -> TestResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let shared_hits = hits.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let shared_active = active.clone();
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let shared_max_in_flight = max_in_flight.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let hits = shared_hits.clone();
                let active = shared_active.clone();
                let max_in_flight = shared_max_in_flight.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        let Ok(count) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..count]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let target = String::from_utf8_lossy(&request)
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    hits.fetch_add(1, Ordering::SeqCst);
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_in_flight.fetch_max(now_active, Ordering::SeqCst);
                    let response = handler(&target);
                    if !response.delay.is_zero() {
                        tokio::time::sleep(response.delay).await;
                    }
                    let location = response
                        .location
                        .as_deref()
                        .map(|value| format!("location: {value}\r\n"))
                        .unwrap_or_default();
                    let head = format!(
                        "HTTP/1.1 {} TEST\r\ncontent-type: {}\r\n{location}content-length: {}\r\nconnection: close\r\n\r\n",
                        response.status,
                        response.content_type,
                        response.body.len()
                    );
                    if stream.write_all(head.as_bytes()).await.is_ok() {
                        let _ = stream.write_all(&response.body).await;
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            address,
            hits,
            max_in_flight,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn url_value(&self, path: &str) -> Url {
        Url::parse(&self.url(path)).unwrap()
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn _assert_send_future<T: Future + Send>(future: T) -> T {
    future
}
