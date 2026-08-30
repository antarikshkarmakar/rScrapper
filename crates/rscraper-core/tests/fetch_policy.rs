mod support;

use reqwest::header::{
    HeaderName, HeaderValue, AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION, WWW_AUTHENTICATE,
};
use rscraper_core::limits::{MAX_CONNECT_TIMEOUT, MAX_REQUEST_TIMEOUT};
use rscraper_core::{
    Error, FetchClient, FetchHostRestriction, FetchMode, FetchRedirect, FetchRequest, FetchStep,
    FetchVia, NetworkPolicy, OperationLimits, Page, RawResponse, RobotsFetchStep,
};
use std::sync::Arc;
use std::time::Duration;
use support::{ResponseBody, StaticResolver, TestResponse, TestServer, TestTlsServer};
use url::Url;

fn short_limits() -> OperationLimits {
    OperationLimits {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(5),
        max_body_bytes: 1024,
        max_output_chars: 1024,
        max_redirects: 10,
    }
}

#[test]
fn public_policy_rejects_forbidden_url_syntax_without_leaking_credentials() {
    for url in [
        "file:///etc/passwd",
        "data:text/plain,secret",
        "ftp://example.test/file",
        "http://user:highly-secret@example.test/",
        "http://example.test:bad-port/",
        "http://?missing-host",
    ] {
        let error = match FetchRequest::request(url) {
            Ok(_) => panic!("forbidden URL was accepted"),
            Err(error) => error,
        };
        assert!(
            matches!(error, Error::InvalidInput(_) | Error::Policy(_)),
            "unexpected error for {url}: {error}"
        );
        assert!(!error.to_string().contains("highly-secret"));
    }
}

#[test]
fn fetch_request_constructors_select_the_documented_modes() {
    assert_eq!(
        FetchRequest::request("https://example.test/").unwrap().mode,
        FetchMode::Request
    );
    assert_eq!(
        FetchRequest::browser("https://example.test/").unwrap().mode,
        FetchMode::Browser
    );
    assert_eq!(
        FetchRequest::auto("https://example.test/").unwrap().mode,
        FetchMode::Auto
    );
}

#[test]
fn request_and_step_debug_diagnostics_redact_all_request_secrets() {
    let mut request = FetchRequest::request("https://example.test/").unwrap();
    request.url = Url::parse(
        "https://request-user:request-password@example.test/request-path-sentinel?credential=request-query-sentinel#request-fragment-sentinel",
    )
    .unwrap();
    request.proxy = Some(
        Url::parse(
            "http://proxy-user:proxy-password@127.0.0.1:8080/proxy-path-sentinel?proxy-query-sentinel#proxy-fragment-sentinel",
        )
        .unwrap(),
    );
    for (name, value) in [
        (AUTHORIZATION, "authorization-secret-sentinel"),
        (COOKIE, "cookie-secret-sentinel"),
        (PROXY_AUTHORIZATION, "proxy-auth-secret-sentinel"),
    ] {
        request
            .headers
            .insert(name, HeaderValue::from_static(value));
    }
    request.headers.insert(
        HeaderName::from_static("x-header-name-secret-sentinel"),
        HeaderValue::from_static("arbitrary-sensitive-sentinel"),
    );
    let mut marked = HeaderValue::from_static("marked-sensitive-sentinel");
    marked.set_sensitive(true);
    request
        .headers
        .insert(HeaderName::from_static("x-marked-secret"), marked);

    let page = Page {
        url: Url::parse(
            "https://page-user:page-password@page.example/page-path-sentinel?page-query-sentinel#page-fragment-sentinel",
        )
        .unwrap(),
        status: 201,
        content_type: Some(
            "text/html; credential=page-content-type-secret-sentinel".into(),
        ),
        html: "page-body-secret-sentinel".into(),
        via: FetchVia::Request,
    };
    let raw = RawResponse {
        url: Url::parse(
            "https://raw-user:raw-password@raw.example/raw-path-sentinel?raw-query-sentinel#raw-fragment-sentinel",
        )
        .unwrap(),
        status: 202,
        content_type: Some(
            "application/octet-stream; credential=raw-content-type-secret-sentinel".into(),
        ),
        bytes: b"raw-bytes-secret-sentinel".to_vec(),
        via: FetchVia::Test,
        rate_limit: Default::default(),
    };
    let diagnostics = [
        ("request", format!("{request:?}")),
        (
            "redirect response",
            format!(
                "{:?}",
                FetchRedirect {
                    status: 302,
                    next_request: request.clone(),
                }
            ),
        ),
        (
            "page redirect step",
            format!(
                "{:?}",
                FetchStep::Redirect(FetchRedirect {
                    status: 303,
                    next_request: request.clone(),
                })
            ),
        ),
        ("page", format!("{page:?}")),
        (
            "page response step",
            format!("{:?}", FetchStep::Response(page.clone())),
        ),
        ("raw response", format!("{raw:?}")),
        (
            "robots redirect step",
            format!(
                "{:?}",
                RobotsFetchStep::Redirect(FetchRedirect {
                    status: 307,
                    next_request: request,
                })
            ),
        ),
        (
            "robots missing step",
            format!(
                "{:?}",
                RobotsFetchStep::Missing {
                    url: Url::parse(
                        "https://missing-user:missing-password@missing.example/missing-path-sentinel?missing-query-sentinel#missing-fragment-sentinel",
                    )
                    .unwrap(),
                }
            ),
        ),
        (
            "robots text step",
            format!(
                "{:?}",
                RobotsFetchStep::Text {
                    url: Url::parse(
                        "https://text-user:text-password@text.example/text-path-sentinel?text-query-sentinel#text-fragment-sentinel",
                    )
                    .unwrap(),
                    text: "robots-text-secret-sentinel".into(),
                }
            ),
        ),
        (
            "robots status step",
            format!(
                "{:?}",
                RobotsFetchStep::Status {
                    url: Url::parse(
                        "https://status-user:status-password@status.example/status-path-sentinel?status-query-sentinel#status-fragment-sentinel",
                    )
                    .unwrap(),
                    status: 503,
                }
            ),
        ),
    ];
    let secrets = [
        "example.test",
        "request-user",
        "request-password",
        "request-path-sentinel",
        "request-query-sentinel",
        "request-fragment-sentinel",
        "proxy-user",
        "proxy-password",
        "127.0.0.1",
        "proxy-path-sentinel",
        "proxy-query-sentinel",
        "proxy-fragment-sentinel",
        "authorization",
        "cookie",
        "proxy-authorization",
        "x-header-name-secret-sentinel",
        "authorization-secret-sentinel",
        "cookie-secret-sentinel",
        "proxy-auth-secret-sentinel",
        "arbitrary-sensitive-sentinel",
        "marked-sensitive-sentinel",
        "page-user",
        "page-password",
        "page.example",
        "page-path-sentinel",
        "page-query-sentinel",
        "page-fragment-sentinel",
        "page-content-type-secret-sentinel",
        "page-body-secret-sentinel",
        "raw-user",
        "raw-password",
        "raw.example",
        "raw-path-sentinel",
        "raw-query-sentinel",
        "raw-fragment-sentinel",
        "raw-content-type-secret-sentinel",
        "raw-bytes-secret-sentinel",
        "missing-user",
        "missing-password",
        "missing.example",
        "missing-path-sentinel",
        "missing-query-sentinel",
        "missing-fragment-sentinel",
        "text-user",
        "text-password",
        "text.example",
        "text-path-sentinel",
        "text-query-sentinel",
        "text-fragment-sentinel",
        "robots-text-secret-sentinel",
        "status-user",
        "status-password",
        "status.example",
        "status-path-sentinel",
        "status-query-sentinel",
        "status-fragment-sentinel",
    ];
    let mut leaks = Vec::new();
    for (kind, diagnostic) in &diagnostics {
        let diagnostic = diagnostic.to_ascii_lowercase();
        for secret in &secrets {
            if diagnostic.contains(&secret.to_ascii_lowercase()) {
                leaks.push(format!("{kind} leaked {secret}: {diagnostic}"));
            }
        }
    }
    assert!(leaks.is_empty(), "{}", leaks.join("\n"));
    let rendered_raw_bytes = format!("{:?}", b"raw-bytes-secret-sentinel".to_vec());
    assert!(
        !diagnostics[5].1.contains(&rendered_raw_bytes),
        "raw response printed its byte payload: {}",
        diagnostics[5].1
    );

    for (_, diagnostic) in diagnostics.iter().take(10) {
        assert!(diagnostic.contains("<redacted>"), "{diagnostic}");
    }
    assert!(diagnostics[0].1.contains("Request"));
    assert!(diagnostics[2].1.contains("303"));
    assert!(diagnostics[3].1.contains("201"));
    assert!(diagnostics[3].1.contains("content_type_present"));
    assert!(diagnostics[3].1.contains("body_len"));
    assert!(diagnostics[5].1.contains("202"));
    assert!(diagnostics[5].1.contains("content_type_present"));
    assert!(diagnostics[5].1.contains("bytes_len"));
    assert!(diagnostics[8].1.contains("text_len"));
    assert!(diagnostics[9].1.contains("503"));
}

#[test]
fn builder_enforces_fixed_timeout_bounds_synchronously() {
    for (name, connect_timeout, request_timeout) in [
        (
            "positive defaults",
            OperationLimits::default().connect_timeout,
            OperationLimits::default().request_timeout,
        ),
        (
            "one nanosecond below both maxima",
            MAX_CONNECT_TIMEOUT - Duration::from_nanos(1),
            MAX_REQUEST_TIMEOUT - Duration::from_nanos(1),
        ),
        (
            "exact fixed maxima",
            MAX_CONNECT_TIMEOUT,
            MAX_REQUEST_TIMEOUT,
        ),
    ] {
        let client = FetchClient::builder()
            .limits(OperationLimits {
                connect_timeout,
                request_timeout,
                ..OperationLimits::default()
            })
            .build()
            .unwrap_or_else(|error| panic!("{name} was rejected: {error}"));
        assert_eq!(client.limits().connect_timeout, connect_timeout);
        assert_eq!(client.limits().request_timeout, request_timeout);
    }

    for (name, connect_timeout, request_timeout) in [
        (
            "zero connect timeout",
            Duration::ZERO,
            Duration::from_secs(1),
        ),
        (
            "zero request timeout",
            Duration::from_secs(1),
            Duration::ZERO,
        ),
        (
            "connect timeout one nanosecond over fixed maximum",
            MAX_CONNECT_TIMEOUT + Duration::from_nanos(1),
            Duration::from_secs(1),
        ),
        (
            "request timeout one nanosecond over fixed maximum",
            Duration::from_secs(1),
            MAX_REQUEST_TIMEOUT + Duration::from_nanos(1),
        ),
        (
            "Duration::MAX connect timeout",
            Duration::MAX,
            Duration::from_secs(1),
        ),
        (
            "Duration::MAX request timeout",
            Duration::from_secs(1),
            Duration::MAX,
        ),
    ] {
        let error = match FetchClient::builder()
            .limits(OperationLimits {
                connect_timeout,
                request_timeout,
                ..OperationLimits::default()
            })
            .build()
        {
            Ok(_) => panic!("{name} was accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::InvalidInput(_)), "{name}: {error}");
    }
}

#[tokio::test]
async fn public_policy_rejects_every_forbidden_literal_address_family() {
    let forbidden = [
        "http://0.0.0.0/",
        "http://10.0.0.1/",
        "http://100.64.0.1/",
        "http://127.0.0.1/",
        "http://2130706433/",
        "http://0177.0.0.1/",
        "http://0x7f000001/",
        "http://169.254.169.254/",
        "http://172.16.0.1/",
        "http://192.0.0.1/",
        "http://192.0.2.1/",
        "http://192.88.99.1/",
        "http://192.168.0.1/",
        "http://198.18.0.1/",
        "http://198.51.100.1/",
        "http://203.0.113.1/",
        "http://224.0.0.1/",
        "http://240.0.0.1/",
        "http://255.255.255.255/",
        "http://[::]/",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://[64:ff9b::7f00:1]/",
        "http://[64:ff9b:1::1]/",
        "http://[100::1]/",
        "http://[2001::1]/",
        "http://[2001:2::1]/",
        "http://[2001:db8::1]/",
        "http://[2001:10::1]/",
        "http://[2001:20::1]/",
        "http://[2002::1]/",
        "http://[fc00::1]/",
        "http://[fe80::1]/",
        "http://[fec0::1]/",
        "http://[ff00::1]/",
    ];
    let client = FetchClient::builder()
        .limits(short_limits())
        .build()
        .unwrap();

    for url in forbidden {
        let error = client
            .fetch_request(FetchRequest::request(url).unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Policy(_)), "{url}: {error}");
    }
}

#[tokio::test]
async fn public_policy_rejects_localhost_names_case_insensitively() {
    let resolver = StaticResolver::single("localhost", vec!["127.0.0.1".parse().unwrap()]);
    let client = FetchClient::builder()
        .limits(short_limits())
        .resolver(Arc::new(resolver))
        .build()
        .unwrap();

    for url in [
        "http://localhost/",
        "http://LOCALHOST./",
        "http://x.localhost/",
    ] {
        let error = client
            .fetch_request(FetchRequest::request(url).unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Policy(_)), "{url}: {error}");
    }
}

#[tokio::test]
async fn public_policy_rejects_mixed_dns_answers() {
    let resolver = StaticResolver::single(
        "mixed.test",
        vec![
            "93.184.216.34".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ],
    );
    let client = FetchClient::builder()
        .limits(short_limits())
        .resolver(Arc::new(resolver))
        .build()
        .unwrap();
    let error = client
        .fetch_request(FetchRequest::request("https://mixed.test/").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Policy(_)));
}

#[tokio::test]
async fn resolver_failures_map_to_typed_dns_errors_for_direct_and_local_socks_fetches() {
    for proxy in [None, Some(Url::parse("socks5://127.0.0.1:9/").unwrap())] {
        let client = FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .limits(short_limits())
            .resolver(Arc::new(StaticResolver::default()))
            .build()
            .unwrap();
        let mut request = FetchRequest::request("http://unresolved.test/resource").unwrap();
        request.proxy = proxy;

        let error = request_error_without_debugging_request(&client, request).await;
        assert!(matches!(error, Error::Dns(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "DNS error: destination resolution failed"
        );
    }
}

#[tokio::test]
async fn allow_private_fetches_a_loopback_fixture() {
    let server = TestServer::spawn([("/", TestResponse::html(200, "local fixture"))]).await;
    let resolver = StaticResolver::single("fixture.test", vec![server.address().ip()]);
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(short_limits())
        .resolver(Arc::new(resolver))
        .build()
        .unwrap();
    let request =
        FetchRequest::request(&format!("http://fixture.test:{}/", server.address().port()))
            .unwrap();

    let page = client.fetch_request(request).await.unwrap();
    assert_eq!(page.html, "local fixture");
}

#[tokio::test]
async fn raw_response_preserves_original_bounded_bytes_without_changing_page_decoding() {
    let iso_8859_1_feed =
        b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><rss><channel><item><title>Caf\xe9</title></item></channel></rss>";
    let server = TestServer::spawn([(
        "/latin1.xml",
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/rss+xml".into())],
            body: ResponseBody::Fixed(iso_8859_1_feed.to_vec()),
        },
    )])
    .await;
    let client = private_client(short_limits());

    let raw = client
        .fetch_raw_request(request_for(&server, "/latin1.xml"))
        .await
        .unwrap();
    let page = client
        .fetch_request(request_for(&server, "/latin1.xml"))
        .await
        .unwrap();

    assert_eq!(raw.status, 200);
    assert_eq!(raw.url, server.url("/latin1.xml"));
    assert_eq!(raw.content_type.as_deref(), Some("application/rss+xml"));
    assert_eq!(raw.bytes, iso_8859_1_feed);
    assert!(page.html.contains("Caf\u{fffd}"));
}

#[tokio::test]
async fn raw_response_exposes_only_typed_final_rate_limit_metadata() {
    let destination = TestServer::spawn([(
        "/final",
        TestResponse {
            status: 429,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Retry-After".into(), "17".into()),
                ("X-RateLimit-Remaining".into(), "0".into()),
                ("X-RateLimit-Reset".into(), "1_800_000_000".replace('_', "")),
                ("Set-Cookie".into(), "session=response-secret".into()),
                ("Authorization".into(), "response-secret".into()),
            ],
            body: ResponseBody::Fixed(br#"{"message":"slow down"}"#.to_vec()),
        },
    )])
    .await;
    let source = TestServer::spawn([(
        "/start",
        TestResponse {
            status: 302,
            headers: vec![
                ("Location".into(), destination.url("/final").to_string()),
                ("Retry-After".into(), "999".into()),
                ("X-RateLimit-Remaining".into(), "88".into()),
                ("Content-Type".into(), "text/html".into()),
            ],
            body: ResponseBody::Fixed(Vec::new()),
        },
    )])
    .await;

    let raw = private_client(short_limits())
        .fetch_raw_request(request_for(&source, "/start"))
        .await
        .unwrap();

    assert_eq!(raw.rate_limit.retry_after_secs, Some(17));
    assert_eq!(raw.rate_limit.remaining, Some(0));
    assert_eq!(raw.rate_limit.reset_epoch_secs, Some(1_800_000_000));
    let diagnostic = format!("{raw:?}");
    assert!(!diagnostic.contains("response-secret"));
    assert!(!diagnostic.contains("set-cookie"));
    assert!(!diagnostic.contains("authorization"));
}

#[tokio::test]
async fn invalid_or_non_numeric_rate_limit_headers_are_not_exposed_as_raw_text() {
    let server = TestServer::spawn([(
        "/invalid",
        TestResponse {
            status: 429,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Retry-After".into(), "response-secret".into()),
                ("X-RateLimit-Remaining".into(), "not-a-number".into()),
                ("X-RateLimit-Reset".into(), "-1".into()),
            ],
            body: ResponseBody::Fixed(b"{}".to_vec()),
        },
    )])
    .await;

    let raw = private_client(short_limits())
        .fetch_raw_request(request_for(&server, "/invalid"))
        .await
        .unwrap();

    assert_eq!(raw.rate_limit.retry_after_secs, None);
    assert_eq!(raw.rate_limit.remaining, None);
    assert_eq!(raw.rate_limit.reset_epoch_secs, None);
    assert!(!format!("{raw:?}").contains("response-secret"));
}

#[tokio::test]
async fn host_restriction_rejects_initial_request_before_traffic() {
    let (server, requests) =
        TestServer::spawn_recording([("/blocked", TestResponse::html(200, "must not fetch"))])
            .await;
    let mut request = request_for(&server, "/blocked");
    request.host_restriction = Some(
        FetchHostRestriction::https_label_suffixes(["youtube.com", "googlevideo.com"]).unwrap(),
    );

    let error = private_client(short_limits())
        .fetch_raw_request(request)
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Policy(_)), "{error}");
    assert!(requests.lock().unwrap().is_empty());
}

fn request_for(server: &TestServer, path: &str) -> FetchRequest {
    FetchRequest::request(server.url(path).as_str()).unwrap()
}

fn private_client(limits: OperationLimits) -> FetchClient {
    FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(limits)
        .build()
        .unwrap()
}

#[tokio::test]
async fn eleven_redirects_exceed_the_ten_hop_limit() {
    let mut routes = Vec::new();
    for hop in 0..11 {
        routes.push((
            format!("/{hop}"),
            TestResponse::redirect(format!("/{}", hop + 1)),
        ));
    }
    routes.push(("/11".into(), TestResponse::html(200, "too far")));
    let server = TestServer::spawn(routes).await;
    let client = private_client(short_limits());

    let error = client
        .fetch_request(request_for(&server, "/0"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error}");
}

#[tokio::test]
async fn not_modified_with_location_is_returned_without_following() {
    let server = TestServer::spawn([
        (
            "/cached",
            TestResponse {
                status: 304,
                headers: vec![
                    ("Location".into(), "/must-not-follow".into()),
                    ("Content-Type".into(), "text/html".into()),
                ],
                body: ResponseBody::Fixed(Vec::new()),
            },
        ),
        (
            "/must-not-follow",
            TestResponse::html(200, "wrong response"),
        ),
    ])
    .await;
    let original_url = server.url("/cached");

    let page = private_client(short_limits())
        .fetch_request(request_for(&server, "/cached"))
        .await
        .unwrap();

    assert_eq!(page.status, 304);
    assert_eq!(page.url, original_url);
    assert!(page.html.is_empty());
}

#[tokio::test]
async fn cross_origin_redirect_strips_standard_and_marked_sensitive_headers() {
    let (destination, requests) =
        TestServer::spawn_recording([("/final", TestResponse::html(200, "arrived"))]).await;
    let redirect = TestServer::spawn([(
        "/start",
        TestResponse::redirect(destination.url("/final").to_string()),
    )])
    .await;
    let mut request = request_for(&redirect, "/start");
    for (name, value) in [
        (AUTHORIZATION, "Bearer authorization-secret"),
        (COOKIE, "session=cookie-secret"),
        (PROXY_AUTHORIZATION, "Basic proxy-secret"),
        (WWW_AUTHENTICATE, "Basic realm=secret"),
    ] {
        request
            .headers
            .insert(name, HeaderValue::from_static(value));
    }
    request.headers.insert(
        HeaderName::from_static("cookie2"),
        HeaderValue::from_static("legacy-cookie-secret"),
    );
    let mut marked_secret = HeaderValue::from_static("custom-sensitive-secret");
    marked_secret.set_sensitive(true);
    request
        .headers
        .insert(HeaderName::from_static("x-api-key"), marked_secret);
    request.headers.insert(
        HeaderName::from_static("x-trace-id"),
        HeaderValue::from_static("ordinary-value"),
    );

    private_client(short_limits())
        .fetch_request(request)
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target, "/final");
    assert_eq!(
        requests[0].headers.get("x-trace-id").map(String::as_str),
        Some("ordinary-value")
    );
    for name in [
        "authorization",
        "cookie",
        "cookie2",
        "proxy-authorization",
        "www-authenticate",
        "x-api-key",
    ] {
        assert!(!requests[0].headers.contains_key(name), "leaked {name}");
    }
}

#[tokio::test]
async fn one_hop_fetch_stops_before_the_redirect_target_and_preserves_header_policy() {
    let (destination, destination_requests) =
        TestServer::spawn_recording([("/final", TestResponse::html(200, "arrived"))]).await;
    let (source, source_requests) = TestServer::spawn_recording([(
        "/start",
        TestResponse::redirect(destination.url("/final").to_string()),
    )])
    .await;
    let mut request = request_for(&source, "/start");
    request.headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer must-not-cross-origin"),
    );
    request.headers.insert(
        HeaderName::from_static("x-trace-id"),
        HeaderValue::from_static("ordinary-value"),
    );
    let client = private_client(short_limits());

    let redirect = match client.fetch_request_one_hop(request).await.unwrap() {
        FetchStep::Redirect(redirect) => redirect,
        FetchStep::Response(_) => panic!("redirect target was followed inside one-hop fetch"),
    };
    assert_eq!(source_requests.lock().unwrap().len(), 1);
    assert!(destination_requests.lock().unwrap().is_empty());
    assert!(!redirect.next_request.headers.contains_key(AUTHORIZATION));
    assert_eq!(
        redirect
            .next_request
            .headers
            .get("x-trace-id")
            .and_then(|value| value.to_str().ok()),
        Some("ordinary-value")
    );

    let page = match client
        .fetch_request_one_hop(redirect.next_request)
        .await
        .unwrap()
    {
        FetchStep::Response(page) => page,
        FetchStep::Redirect(_) => panic!("unexpected second redirect"),
    };
    assert_eq!(page.html, "arrived");
    assert_eq!(destination_requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn robots_fetch_exposes_headerless_404_before_mime_or_body_processing() {
    let server = TestServer::spawn([(
        "/robots.txt",
        TestResponse {
            status: 404,
            headers: Vec::new(),
            body: ResponseBody::Delayed {
                delay: Duration::from_secs(5),
                bytes: vec![0xff; 128],
            },
        },
    )])
    .await;

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        private_client(short_limits()).fetch_robots_one_hop(request_for(&server, "/robots.txt")),
    )
    .await
    .expect("robots 404 waited for its body")
    .unwrap();

    assert!(matches!(result, RobotsFetchStep::Missing { .. }));
}

#[tokio::test]
async fn successful_robots_text_is_mime_validated_and_body_bounded() {
    let server = TestServer::spawn([
        (
            "/valid",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
                body: ResponseBody::Fixed(b"User-agent: *\nDisallow: /private\n".to_vec()),
            },
        ),
        ("/html", TestResponse::html(200, "not robots text")),
        (
            "/large",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: ResponseBody::Fixed(vec![b'x'; 1_025]),
            },
        ),
    ])
    .await;
    let client = private_client(short_limits());

    match client
        .fetch_robots_one_hop(request_for(&server, "/valid"))
        .await
        .unwrap()
    {
        RobotsFetchStep::Text { text, .. } => assert!(text.contains("Disallow")),
        other => panic!("unexpected robots result: {other:?}"),
    }
    assert!(matches!(
        client
            .fetch_robots_one_hop(request_for(&server, "/html"))
            .await
            .unwrap_err(),
        Error::Policy(_)
    ));
    assert!(matches!(
        client
            .fetch_robots_one_hop(request_for(&server, "/large"))
            .await
            .unwrap_err(),
        Error::BodyLimit { limit: 1_024 }
    ));
}

#[tokio::test]
async fn delayed_body_exceeding_request_timeout_terminates_promptly() {
    let server = TestServer::spawn([(
        "/slow",
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/html".into())],
            body: ResponseBody::Delayed {
                delay: Duration::from_secs(5),
                bytes: b"late body".to_vec(),
            },
        },
    )])
    .await;
    let mut limits = short_limits();
    limits.request_timeout = Duration::from_millis(50);
    let client = private_client(limits);

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.fetch_request(request_for(&server, "/slow")),
    )
    .await
    .expect("fetch did not terminate promptly")
    .unwrap_err();
    assert!(matches!(
        result,
        Error::Timeout {
            operation: "request"
        }
    ));
}

#[tokio::test]
async fn non_success_html_preserves_status_and_body() {
    let server = TestServer::spawn([("/missing", TestResponse::html(404, "missing page"))]).await;
    let client = private_client(short_limits());

    let page = client
        .fetch_request(request_for(&server, "/missing"))
        .await
        .unwrap();
    assert_eq!(page.status, 404);
    assert_eq!(page.html, "missing page");
    assert_eq!(
        page.content_type.as_deref(),
        Some("text/html; charset=utf-8")
    );
}

#[tokio::test]
async fn accepted_document_media_families_are_returned() {
    let server = TestServer::spawn([
        ("/html", typed_response("text/html", "html")),
        ("/xhtml", typed_response("application/xhtml+xml", "xhtml")),
        ("/xml", typed_response("application/xml", "xml")),
        ("/json", typed_response("application/json", "json")),
        (
            "/problem-json",
            typed_response("application/problem+json", "problem"),
        ),
        ("/atom-xml", typed_response("application/atom+xml", "atom")),
        ("/text", typed_response("text/plain", "text")),
    ])
    .await;
    let client = private_client(short_limits());

    for (path, expected) in [
        ("/html", "html"),
        ("/xhtml", "xhtml"),
        ("/xml", "xml"),
        ("/json", "json"),
        ("/problem-json", "problem"),
        ("/atom-xml", "atom"),
        ("/text", "text"),
    ] {
        let page = client
            .fetch_request(request_for(&server, path))
            .await
            .unwrap();
        assert_eq!(page.html, expected, "{path}");
    }
}

#[tokio::test]
async fn missing_content_type_is_rejected_before_reading_body_without_leaking_it() {
    let server = TestServer::spawn([(
        "/untyped",
        TestResponse {
            status: 200,
            headers: Vec::new(),
            body: ResponseBody::Delayed {
                delay: Duration::from_secs(5),
                bytes: b"untyped-binary-secret".to_vec(),
            },
        },
    )])
    .await;

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        private_client(short_limits()).fetch_request(request_for(&server, "/untyped")),
    )
    .await
    .expect("missing content type rejection waited for the response body")
    .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error}");
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains("untyped-binary-secret"));
}

#[tokio::test]
async fn duplicate_content_type_fields_with_conflicting_charsets_are_rejected() {
    let server = TestServer::spawn([(
        "/conflicting-charsets",
        TestResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/html; charset=utf-8".into()),
                (
                    "Content-Type".into(),
                    "text/html; charset=windows-1252".into(),
                ),
            ],
            body: ResponseBody::Fixed(vec![0xe9]),
        },
    )])
    .await;

    let error = private_client(short_limits())
        .fetch_request(request_for(&server, "/conflicting-charsets"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error}");
}

#[tokio::test]
async fn duplicate_content_type_fields_accept_semantically_identical_charsets() {
    let server = TestServer::spawn([(
        "/equivalent-charsets",
        TestResponse {
            status: 200,
            headers: vec![
                (
                    "Content-Type".into(),
                    "Text/HTML; Charset=windows-1252".into(),
                ),
                (
                    "Content-Type".into(),
                    "text/html; charset=iso-8859-1".into(),
                ),
            ],
            body: ResponseBody::Fixed(vec![b'c', b'a', b'f', 0xe9]),
        },
    )])
    .await;

    let page = private_client(short_limits())
        .fetch_request(request_for(&server, "/equivalent-charsets"))
        .await
        .unwrap();
    assert_eq!(page.html, "café");
}

#[tokio::test]
async fn repeated_conflicting_charset_parameters_are_rejected_before_body_without_leaking_it() {
    let server = TestServer::spawn([(
        "/conflicting-parameters",
        TestResponse {
            status: 200,
            headers: vec![(
                "Content-Type".into(),
                "text/html; charset=utf-8; charset=windows-1252".into(),
            )],
            body: ResponseBody::Delayed {
                delay: Duration::from_secs(5),
                bytes: b"conflicting-charset-secret".to_vec(),
            },
        },
    )])
    .await;

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        private_client(short_limits())
            .fetch_request(request_for(&server, "/conflicting-parameters")),
    )
    .await
    .expect("conflicting charset rejection waited for the response body")
    .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error}");
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains("conflicting-charset-secret"));
}

#[tokio::test]
async fn repeated_semantically_identical_charset_aliases_select_the_canonical_decoder() {
    let server = TestServer::spawn([(
        "/equivalent-parameters",
        TestResponse {
            status: 200,
            headers: vec![(
                "Content-Type".into(),
                "text/html; charset=iso-8859-1; charset=windows-1252".into(),
            )],
            body: ResponseBody::Fixed(vec![b'c', b'a', b'f', 0xe9]),
        },
    )])
    .await;

    let page = private_client(short_limits())
        .fetch_request(request_for(&server, "/equivalent-parameters"))
        .await
        .unwrap();
    assert_eq!(page.html, "café");
}

#[tokio::test]
async fn content_type_parameter_without_value_is_rejected_before_body() {
    assert_malformed_content_type_is_rejected("text/html; charset").await;
}

#[tokio::test]
async fn unterminated_quoted_content_type_parameter_is_rejected_before_body() {
    assert_malformed_content_type_is_rejected("text/html; charset=\"unterminated").await;
}

#[tokio::test]
async fn empty_trailing_content_type_parameter_list_is_rejected_before_body() {
    assert_malformed_content_type_is_rejected("text/html;").await;
}

async fn assert_malformed_content_type_is_rejected(content_type: &str) {
    let server = TestServer::spawn([(
        "/malformed-parameter",
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: ResponseBody::Delayed {
                delay: Duration::from_secs(5),
                bytes: b"must-not-be-read".to_vec(),
            },
        },
    )])
    .await;

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        private_client(short_limits()).fetch_request(request_for(&server, "/malformed-parameter")),
    )
    .await
    .expect("malformed content type rejection waited for the response body")
    .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error}");
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains("must-not-be-read"));
}

#[tokio::test]
async fn unsupported_media_type_is_rejected_before_reading_body() {
    let server = TestServer::spawn([(
        "/binary",
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/octet-stream".into())],
            body: ResponseBody::Delayed {
                delay: Duration::from_secs(5),
                bytes: b"binary payload".to_vec(),
            },
        },
    )])
    .await;

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        private_client(short_limits()).fetch_request(request_for(&server, "/binary")),
    )
    .await
    .expect("media policy waited for the response body")
    .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error}");
}

#[tokio::test]
async fn non_document_media_families_are_rejected() {
    let server = TestServer::spawn([
        ("/svg", typed_response("image/svg+xml", "svg")),
        ("/pdf", typed_response("application/pdf", "pdf")),
        ("/multipart", typed_response("multipart/mixed", "multipart")),
        ("/malformed", typed_response("text/ html", "malformed")),
    ])
    .await;
    let client = private_client(short_limits());

    for path in ["/svg", "/pdf", "/multipart", "/malformed"] {
        let error = client
            .fetch_request(request_for(&server, path))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Policy(_)), "{path}: {error}");
    }
}

#[tokio::test]
async fn wildcard_and_pseudo_suffix_media_types_are_rejected_before_body() {
    for content_type in [
        "application/+json",
        "application/*+json",
        "application/+xml",
        "application/*+xml",
        "text/*",
    ] {
        let server = TestServer::spawn([(
            "/non-concrete",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), content_type.into())],
                body: ResponseBody::Delayed {
                    delay: Duration::from_secs(5),
                    bytes: b"pseudo-media-secret".to_vec(),
                },
            },
        )])
        .await;

        let error = tokio::time::timeout(
            Duration::from_secs(2),
            private_client(short_limits()).fetch_request(request_for(&server, "/non-concrete")),
        )
        .await
        .expect("non-concrete media rejection waited for the response body")
        .unwrap_err();
        assert!(matches!(error, Error::Policy(_)), "{content_type}: {error}");
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("pseudo-media-secret"));
    }
}

#[tokio::test]
async fn attachment_is_rejected_before_reading_body() {
    let server = TestServer::spawn([(
        "/download",
        TestResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/html".into()),
                (
                    "Content-Disposition".into(),
                    "attachment; filename=secret.html".into(),
                ),
            ],
            body: ResponseBody::Delayed {
                delay: Duration::from_secs(5),
                bytes: b"download".to_vec(),
            },
        },
    )])
    .await;

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        private_client(short_limits()).fetch_request(request_for(&server, "/download")),
    )
    .await
    .expect("attachment policy waited for the response body")
    .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error}");
}

fn typed_response(content_type: &str, body: &str) -> TestResponse {
    TestResponse {
        status: 200,
        headers: vec![("Content-Type".into(), content_type.into())],
        body: ResponseBody::Fixed(body.as_bytes().to_vec()),
    }
}

#[tokio::test]
async fn oversized_content_length_is_rejected_before_reading_the_body() {
    let server = TestServer::spawn([(
        "/large-length",
        TestResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/html".into()),
                ("Content-Length".into(), "1000".into()),
            ],
            body: ResponseBody::Delayed {
                delay: Duration::from_secs(5),
                bytes: b"small fixture body".to_vec(),
            },
        },
    )])
    .await;
    let mut limits = short_limits();
    limits.max_body_bytes = 16;
    let client = private_client(limits);

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        client.fetch_request(request_for(&server, "/large-length")),
    )
    .await
    .expect("content-length rejection waited for the body")
    .unwrap_err();
    assert!(matches!(error, Error::BodyLimit { limit: 16 }));
}

#[tokio::test]
async fn chunked_body_is_stopped_as_soon_as_it_crosses_the_limit() {
    let server = TestServer::spawn([(
        "/chunks",
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: ResponseBody::Chunks(vec![
                (Duration::ZERO, b"abcd".to_vec()),
                (Duration::ZERO, b"efgh".to_vec()),
                (Duration::from_secs(5), b"must not be read".to_vec()),
            ]),
        },
    )])
    .await;
    let mut limits = short_limits();
    limits.max_body_bytes = 6;
    let client = private_client(limits);

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        client.fetch_request(request_for(&server, "/chunks")),
    )
    .await
    .expect("body-limit rejection did not terminate at the crossing chunk")
    .unwrap_err();
    assert!(matches!(error, Error::BodyLimit { limit: 6 }));
}

const GZIP_64_A: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 75, 76, 164, 12, 0, 0, 85, 101, 180, 137, 64, 0, 0, 0,
];
const GZIP_128_A: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 75, 76, 28, 88, 0, 0, 140, 54, 43, 241, 128, 0, 0, 0,
];
const GZIP_MISSING: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 75, 206, 207, 45, 40, 74, 45, 46, 78, 77, 81, 200, 205, 44,
    46, 206, 204, 75, 7, 0, 232, 11, 84, 103, 18, 0, 0, 0,
];

#[tokio::test]
async fn compressed_body_limit_applies_to_decoded_bytes() {
    let server =
        TestServer::spawn([("/compressed-over-limit", gzip_response(200, GZIP_128_A))]).await;
    let mut limits = short_limits();
    limits.max_body_bytes = 64;

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        private_client(limits).fetch_request(request_for(&server, "/compressed-over-limit")),
    )
    .await
    .expect("decoded body limit did not terminate promptly")
    .unwrap_err();
    assert!(matches!(error, Error::BodyLimit { limit: 64 }));
}

#[tokio::test]
async fn compressed_body_exactly_at_decoded_limit_is_accepted() {
    let server =
        TestServer::spawn([("/compressed-exact-limit", gzip_response(200, GZIP_64_A))]).await;
    let mut limits = short_limits();
    limits.max_body_bytes = 64;

    let page = private_client(limits)
        .fetch_request(request_for(&server, "/compressed-exact-limit"))
        .await
        .unwrap();
    assert_eq!(page.html, "a".repeat(64));
}

#[tokio::test]
async fn compressed_non_success_response_preserves_status_and_decoded_body() {
    let server =
        TestServer::spawn([("/compressed-missing", gzip_response(404, GZIP_MISSING))]).await;

    let page = private_client(short_limits())
        .fetch_request(request_for(&server, "/compressed-missing"))
        .await
        .unwrap();
    assert_eq!(page.status, 404);
    assert_eq!(page.html, "compressed missing");
}

fn gzip_response(status: u16, bytes: &[u8]) -> TestResponse {
    TestResponse {
        status,
        headers: vec![
            ("Content-Type".into(), "text/html".into()),
            ("Content-Encoding".into(), "gzip".into()),
        ],
        body: ResponseBody::Fixed(bytes.to_vec()),
    }
}

#[tokio::test]
async fn response_body_uses_the_declared_charset() {
    let server = TestServer::spawn([(
        "/charset",
        TestResponse {
            status: 200,
            headers: vec![(
                "Content-Type".into(),
                "text/html; charset=windows-1252".into(),
            )],
            body: ResponseBody::Fixed(vec![b'c', b'a', b'f', 0xe9]),
        },
    )])
    .await;
    let client = private_client(short_limits());

    let page = client
        .fetch_request(request_for(&server, "/charset"))
        .await
        .unwrap();
    assert_eq!(page.html, "café");
}

#[tokio::test]
async fn allow_private_uses_an_explicit_loopback_proxy() {
    let target = "http://93.184.216.34/proxied";
    let proxy = TestServer::spawn([(target, TestResponse::html(200, "via proxy"))]).await;
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(short_limits())
        .build()
        .unwrap();
    let mut request = FetchRequest::request(target).unwrap();
    request.proxy = Some(proxy.url("/"));

    let page = client.fetch_request(request).await.unwrap();
    assert_eq!(page.html, "via proxy");
}

#[tokio::test]
async fn public_policy_rejects_forbidden_literal_proxy_endpoints() {
    let target = "http://93.184.216.34/proxied";
    let loopback = TestServer::spawn([(target, TestResponse::html(200, "private proxy"))]).await;
    let port = loopback.address().port();
    let proxies = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://2130706433:{port}/"),
        "socks5://127.0.0.1:9/".into(),
        "http://10.0.0.1:9/".into(),
        "http://169.254.169.254:9/".into(),
        "http://[fc00::1]:9/".into(),
        "http://[fe80::1]:9/".into(),
    ];
    let client = FetchClient::builder()
        .limits(short_limits())
        .build()
        .unwrap();

    for proxy in proxies {
        let mut request = FetchRequest::request(target).unwrap();
        request.proxy = Some(Url::parse(&proxy).unwrap());
        let error = request_error_without_debugging_request(&client, request).await;
        assert!(matches!(error, Error::Policy(_)), "{proxy}: {error}");
    }
}

#[tokio::test]
async fn public_policy_rejects_mixed_proxy_dns_answers() {
    let target = "http://93.184.216.34/proxied";
    let resolver = StaticResolver::single(
        "proxy.test",
        vec![
            "93.184.216.34".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ],
    );
    let client = FetchClient::builder()
        .limits(short_limits())
        .resolver(Arc::new(resolver))
        .build()
        .unwrap();
    let mut request = FetchRequest::request(target).unwrap();
    request.proxy = Some(Url::parse("http://proxy.test:8080/").unwrap());

    let error = request_error_without_debugging_request(&client, request).await;
    assert!(matches!(error, Error::Policy(_)), "{error}");
}

#[tokio::test]
async fn public_redirect_to_a_resolver_mapped_private_host_is_rejected() {
    let start = "http://93.184.216.34/start";
    let proxy = TestServer::spawn([(
        start,
        TestResponse::redirect("http://private.test/metadata"),
    )])
    .await;
    let resolver = StaticResolver::single("private.test", vec!["127.0.0.1".parse().unwrap()]);
    let client = FetchClient::builder()
        .limits(short_limits())
        .resolver(Arc::new(resolver))
        .build()
        .unwrap();
    let mut request = FetchRequest::request(start).unwrap();
    request.proxy = Some(proxy.url("/"));

    let error = request_error_without_debugging_request(&client, request).await;
    assert!(matches!(error, Error::Policy(_)), "{error}");
}

#[tokio::test]
async fn public_policy_rejects_remote_proxy_dns_for_hostname_destinations() {
    let target = "http://public.test/resource";
    let proxy = TestServer::spawn([(target, TestResponse::html(200, "unsafe remote DNS"))]).await;
    let resolver = StaticResolver::single("public.test", vec!["93.184.216.34".parse().unwrap()]);
    let client = FetchClient::builder()
        .limits(short_limits())
        .resolver(Arc::new(resolver))
        .build()
        .unwrap();
    let mut request = FetchRequest::request(target).unwrap();
    request.proxy = Some(proxy.url("/"));

    let error = request_error_without_debugging_request(&client, request).await;
    assert!(matches!(error, Error::Policy(_)), "{error}");
}

#[test]
fn public_hostname_remote_dns_proxy_incompatibility_is_a_synchronous_preflight_error() {
    let client = FetchClient::builder()
        .limits(short_limits())
        .build()
        .unwrap();

    for proxy in [
        "http://1.1.1.1:8080/",
        "https://1.1.1.1:8080/",
        "socks4a://1.1.1.1:1080/",
        "socks5h://1.1.1.1:1080/",
    ] {
        let mut request = FetchRequest::request("https://public.test/resource").unwrap();
        request.proxy = Some(Url::parse(proxy).unwrap());
        let error = client.preflight_request(&request).unwrap_err();
        assert!(matches!(error, Error::Policy(_)), "{proxy}: {error}");
    }
}

#[tokio::test]
async fn proxy_credentials_and_header_tokens_never_appear_in_errors() {
    let client = FetchClient::builder()
        .limits(short_limits())
        .build()
        .unwrap();
    let mut request =
        FetchRequest::request("http://93.184.216.34/secret-test?access_token=query-secret")
            .unwrap();
    request.proxy = Some(Url::parse("http://proxy-user:proxy-secret@127.0.0.1:9/").unwrap());
    request.headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_static("Bearer header-secret"),
    );

    let error = request_error_without_debugging_request(&client, request).await;
    let display = error.to_string();
    let debug = format!("{error:?}");
    for secret in [
        "proxy-user",
        "proxy-secret",
        "header-secret",
        "query-secret",
    ] {
        assert!(!display.contains(secret), "Display leaked {secret}");
        assert!(!debug.contains(secret), "Debug leaked {secret}");
    }
}

#[tokio::test]
async fn invalid_tls_certificate_is_rejected() {
    let server = TestTlsServer::spawn(TestResponse::html(200, "must not be trusted")).await;
    let resolver = StaticResolver::single("tls.test", vec![server.address().ip()]);
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(short_limits())
        .resolver(Arc::new(resolver))
        .build()
        .unwrap();

    let error = request_error_without_debugging_request(
        &client,
        FetchRequest::request(server.url().as_str()).unwrap(),
    )
    .await;
    let diagnostic = format!("{error:?} {error}").to_ascii_lowercase();
    assert!(matches!(error, Error::Http(_)), "{diagnostic}");
    assert!(
        diagnostic.contains("certificate")
            || diagnostic.contains("unknownissuer")
            || diagnostic.contains("unknown issuer"),
        "unexpected TLS diagnostic: {diagnostic}"
    );
}

async fn request_error_without_debugging_request(
    client: &FetchClient,
    request: FetchRequest,
) -> Error {
    match client.fetch_request(request).await {
        Ok(_) => panic!("fixture request unexpectedly succeeded"),
        Err(error) => error,
    }
}
