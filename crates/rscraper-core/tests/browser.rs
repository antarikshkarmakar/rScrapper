mod support;

use rscraper_core::browser::discover_chromium_executable;
use rscraper_core::{
    looks_like_javascript_shell, BrowserBackend, BrowserEgress, BrowserRenderer, Error,
    FetchClient, FetchRequest, FetchVia, NetworkPolicy, OperationLimits, Page, Result,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{ResponseBody, StaticResolver, TestResponse, TestServer, TestTlsServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use url::Url;

fn browser_test_limits() -> OperationLimits {
    OperationLimits {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(2),
        max_body_bytes: 1024 * 1024,
        max_output_chars: 1024 * 1024,
        max_redirects: 10,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SocksRequest {
    host: String,
    port: u16,
    address_type: u8,
}

#[derive(Clone, Copy)]
enum SocksSpyMode {
    Forward(SocketAddr),
    Reject,
}

struct SocksSpy {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<SocksRequest>>>,
    task: JoinHandle<()>,
}

impl SocksSpy {
    async fn forwarding_to(upstream: SocketAddr) -> Self {
        Self::spawn(SocksSpyMode::Forward(upstream)).await
    }

    async fn rejecting() -> Self {
        Self::spawn(SocksSpyMode::Reject).await
    }

    async fn spawn(mode: SocksSpyMode) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let observed = Arc::clone(&observed);
                tokio::spawn(async move {
                    let _ = handle_socks_spy_connection(stream, mode, observed).await;
                });
            }
        });
        Self {
            address,
            requests,
            task,
        }
    }

    fn proxy_url(&self) -> Url {
        Url::parse(&format!("socks5h://{}/", self.address)).unwrap()
    }

    fn requests(&self) -> Vec<SocksRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for SocksSpy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_socks_spy_connection(
    mut client: TcpStream,
    mode: SocksSpyMode,
    observed: Arc<Mutex<Vec<SocksRequest>>>,
) -> std::io::Result<()> {
    let mut greeting = [0_u8; 2];
    client.read_exact(&mut greeting).await?;
    if greeting[0] != 5 || greeting[1] == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid SOCKS5 greeting",
        ));
    }
    let mut methods = vec![0; usize::from(greeting[1])];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        client.write_all(&[5, 0xff]).await?;
        return Ok(());
    }
    client.write_all(&[5, 0]).await?;

    let mut request = [0_u8; 4];
    client.read_exact(&mut request).await?;
    if request[..3] != [5, 1, 0] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid SOCKS5 CONNECT request",
        ));
    }
    let address_type = request[3];
    let host = match address_type {
        1 => {
            let mut octets = [0; 4];
            client.read_exact(&mut octets).await?;
            IpAddr::V4(Ipv4Addr::from(octets)).to_string()
        }
        3 => {
            let length = client.read_u8().await?;
            let mut domain = vec![0; usize::from(length)];
            client.read_exact(&mut domain).await?;
            String::from_utf8(domain).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid SOCKS domain")
            })?
        }
        4 => {
            let mut octets = [0; 16];
            client.read_exact(&mut octets).await?;
            IpAddr::V6(Ipv6Addr::from(octets)).to_string()
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid SOCKS address type",
            ));
        }
    };
    let port = client.read_u16().await?;
    observed.lock().unwrap().push(SocksRequest {
        host,
        port,
        address_type,
    });

    let SocksSpyMode::Forward(upstream) = mode else {
        client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    };
    let mut upstream = TcpStream::connect(upstream).await?;
    client.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

#[test]
fn empty_body_is_a_javascript_shell() {
    assert!(looks_like_javascript_shell("  \n\t"));
    assert!(looks_like_javascript_shell(
        "<html><head></head><body></body></html>"
    ));
}

#[test]
fn enable_javascript_notice_is_a_javascript_shell() {
    let html =
        "<html><body><noscript>Please enable JavaScript to continue.</noscript></body></html>";
    assert!(looks_like_javascript_shell(html));
}

#[test]
fn cloudflare_style_challenge_is_a_javascript_shell() {
    let html = r#"
        <html>
          <head><title>Just a moment...</title></head>
          <body>
            <main>Checking your browser before accessing the site.</main>
            <script>window._cf_chl_opt = {};</script>
          </body>
        </html>
    "#;
    assert!(looks_like_javascript_shell(html));
}

#[test]
fn legitimate_short_page_is_not_a_javascript_shell() {
    let html = r#"
        <html>
          <head><title>Contact Acme</title></head>
          <body><main><h1>Contact Acme</h1><p>Email our support team for help.</p></main></body>
        </html>
    "#;
    assert!(!looks_like_javascript_shell(html));
}

#[test]
fn substantial_page_is_not_a_javascript_shell() {
    let prose = std::iter::repeat_n("substantial", 300)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(!looks_like_javascript_shell(&format!(
        "<html><body><article>{prose}</article></body></html>"
    )));
}

#[derive(Default)]
struct RecordingBrowser {
    calls: Mutex<Vec<(Url, Option<Url>)>>,
    fail: bool,
}

impl RecordingBrowser {
    fn failing() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail: true,
        }
    }

    fn calls(&self) -> Vec<(Url, Option<Url>)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl BrowserBackend for RecordingBrowser {
    async fn render(&self, request: &FetchRequest, _limits: &OperationLimits) -> Result<Page> {
        self.calls
            .lock()
            .unwrap()
            .push((request.url.clone(), request.proxy.clone()));
        if self.fail {
            return Err(Error::Browser("fixture renderer failed".into()));
        }
        Ok(Page {
            url: request.url.clone(),
            status: 200,
            content_type: Some("text/html; charset=utf-8".into()),
            html: "<html><body>rendered-ok</body></html>".into(),
            via: FetchVia::Browser,
        })
    }
}

#[tokio::test]
async fn auto_renders_an_eligible_html_shell_once_with_exact_url_and_proxy() {
    let target = Url::parse("http://93.184.216.34/shell").unwrap();
    let proxy = TestServer::spawn([(
        target.as_str(),
        TestResponse::html(200, "<html><body>Please enable JavaScript</body></html>"),
    )])
    .await;
    let proxy_url = proxy.url("/");
    let browser = Arc::new(RecordingBrowser::default());
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(browser_test_limits())
        .browser(browser.clone())
        .build()
        .unwrap();
    let mut request = FetchRequest::auto(target.as_str()).unwrap();
    request.proxy = Some(proxy_url.clone());

    let page = client.fetch_request(request).await.unwrap();

    assert_eq!(page.via, FetchVia::Browser);
    assert_eq!(page.html, "<html><body>rendered-ok</body></html>");
    assert_eq!(browser.calls(), vec![(target, Some(proxy_url))]);
}

#[tokio::test]
async fn auto_returns_the_original_http_page_when_rendering_fails() {
    let shell = "<html><body>Please enable JavaScript</body></html>";
    let server = TestServer::spawn([("/shell", TestResponse::html(200, shell))]).await;
    let browser = Arc::new(RecordingBrowser::failing());
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(browser_test_limits())
        .browser(browser.clone())
        .build()
        .unwrap();

    let page = client
        .fetch_request(FetchRequest::auto(server.url("/shell").as_str()).unwrap())
        .await
        .unwrap();

    assert_eq!(page.via, FetchVia::Request);
    assert_eq!(page.status, 200);
    assert_eq!(page.html, shell);
    assert_eq!(browser.calls().len(), 1);
}

#[tokio::test]
async fn auto_preserves_request_errors_without_rendering() {
    let browser = Arc::new(RecordingBrowser::default());
    let client = FetchClient::builder()
        .limits(browser_test_limits())
        .resolver(Arc::new(StaticResolver::default()))
        .browser(browser.clone())
        .build()
        .unwrap();

    let error = client
        .fetch_request(FetchRequest::auto("http://unresolved.test/").unwrap())
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Dns(_)), "{error:?}");
    assert!(browser.calls().is_empty());
}

#[tokio::test]
async fn auto_does_not_render_non_success_or_non_html_responses() {
    let server = TestServer::spawn([
        (
            "/missing",
            TestResponse::html(404, "<html><body>Please enable JavaScript</body></html>"),
        ),
        (
            "/plain",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: support::ResponseBody::Fixed(Vec::new()),
            },
        ),
    ])
    .await;
    let browser = Arc::new(RecordingBrowser::default());
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(browser_test_limits())
        .browser(browser.clone())
        .build()
        .unwrap();

    for path in ["/missing", "/plain"] {
        let page = client
            .fetch_request(FetchRequest::auto(server.url(path).as_str()).unwrap())
            .await
            .unwrap();
        assert_eq!(page.via, FetchVia::Request, "{path}");
    }
    assert!(browser.calls().is_empty());
}

#[tokio::test]
async fn request_mode_never_renders_and_browser_mode_requires_a_backend() {
    let shell = "<html><body>Please enable JavaScript</body></html>";
    let server = TestServer::spawn([("/shell", TestResponse::html(200, shell))]).await;
    let browser = Arc::new(RecordingBrowser::default());
    let request_client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(browser_test_limits())
        .browser(browser.clone())
        .build()
        .unwrap();

    let page = request_client
        .fetch_request(FetchRequest::request(server.url("/shell").as_str()).unwrap())
        .await
        .unwrap();
    assert_eq!(page.via, FetchVia::Request);
    assert!(browser.calls().is_empty());

    let browserless_client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(browser_test_limits())
        .build()
        .unwrap();
    let error = browserless_client
        .fetch_request(FetchRequest::browser(server.url("/shell").as_str()).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Browser(_)), "{error:?}");
}

#[test]
fn tor_required_rejects_every_non_socks5h_or_unvalidated_proxy() {
    for proxy in [
        "socks5://127.0.0.1:9050",
        "http://127.0.0.1:8080",
        "socks5h://proxy.test:9050",
        "socks5h://127.0.0.1",
        "socks5h://127.0.0.1:9050/path",
    ] {
        let error = BrowserRenderer::discover(BrowserEgress::TorRequired {
            proxy: Url::parse(proxy).unwrap(),
        })
        .unwrap_err();
        assert!(
            matches!(error, Error::InvalidInput(_) | Error::Policy(_)),
            "{proxy}: {error:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn chromium_renders_local_javascript_and_cleans_its_profile() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let fixture = include_str!("fixtures/js-page.html");
    let server = TestServer::spawn([("/", TestResponse::html(200, fixture))]).await;
    let renderer = Arc::new(
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap(),
    );
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(browser_test_limits())
        .browser(renderer.clone())
        .build()
        .unwrap();

    let page = client
        .fetch_request(FetchRequest::browser(server.url("/").as_str()).unwrap())
        .await
        .unwrap();

    assert_eq!(page.via, FetchVia::Browser);
    assert_eq!(page.status, 200);
    assert_eq!(page.url, server.url("/"));
    assert_eq!(
        page.content_type.as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert!(page.html.contains("rendered-ok"), "{}", page.html);
    let profile = renderer
        .last_profile_path()
        .expect("renderer did not record its temporary profile");
    assert!(
        !profile.exists(),
        "profile remains at {}",
        profile.display()
    );
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn chromium_timeout_reaps_the_child_and_removes_its_profile() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let endless = r#"
        <!doctype html><html><body>
        <script>while (true) { /* deliberately block the renderer */ }</script>
        </body></html>
    "#;
    let server = TestServer::spawn([("/", TestResponse::html(200, endless))]).await;
    let renderer = Arc::new(
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap(),
    );
    let mut limits = browser_test_limits();
    limits.request_timeout = Duration::from_millis(750);
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(limits)
        .browser(renderer.clone())
        .build()
        .unwrap();

    let error = tokio::time::timeout(
        Duration::from_secs(10),
        client.fetch_request(FetchRequest::browser(server.url("/").as_str()).unwrap()),
    )
    .await
    .expect("browser timeout did not terminate promptly")
    .unwrap_err();

    assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
    let profile = renderer
        .last_profile_path()
        .expect("renderer did not record its temporary profile");
    assert!(
        !profile.exists(),
        "profile remains at {}",
        profile.display()
    );
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn tor_required_uses_the_proxy_and_does_not_fall_back_directly() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    for (target, expected_host, expected_address_type) in [
        (
            "http://fixture-without-dns.onion:8080/",
            "fixture-without-dns.onion",
            3,
        ),
        ("http://public-fixture.test:8081/", "public-fixture.test", 3),
        ("http://93.184.216.34:8082/", "93.184.216.34", 3),
    ] {
        let proxy = SocksSpy::rejecting().await;
        let proxy_url = proxy.proxy_url();
        let renderer = BrowserRenderer::discover(BrowserEgress::TorRequired {
            proxy: proxy_url.clone(),
        })
        .unwrap();
        let mut request = FetchRequest::browser(target).unwrap();
        request.proxy = Some(proxy_url);

        let error = renderer
            .render(&request, &browser_test_limits())
            .await
            .unwrap_err();

        assert!(
            matches!(error, Error::Browser(_) | Error::Timeout { .. }),
            "{target}: {error:?}"
        );
        let observed = proxy.requests();
        assert!(
            !observed.is_empty(),
            "SOCKS spy saw no request for {target}"
        );
        assert!(
            observed.iter().all(|request| request.host == expected_host
                && request.port == Url::parse(target).unwrap().port().unwrap()
                && request.address_type == expected_address_type),
            "unexpected SOCKS request or fallback after rejection for {target}: {observed:?}"
        );
        let profile = renderer
            .last_profile_path()
            .expect("renderer did not record its temporary profile");
        assert!(
            !profile.exists(),
            "profile remains at {}",
            profile.display()
        );
        assert!(!renderer.has_active_child());
    }
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn chromium_rejects_invalid_tls_and_still_cleans_up() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let server = TestTlsServer::spawn(TestResponse::html(200, "must-not-render")).await;
    let url = Url::parse(&format!("https://127.0.0.1:{}/", server.address().port())).unwrap();
    let renderer = Arc::new(
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap(),
    );
    let client = FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(browser_test_limits())
        .browser(renderer.clone())
        .build()
        .unwrap();

    let error = client
        .fetch_request(FetchRequest::browser(url.as_str()).unwrap())
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Browser(_)), "{error:?}");
    let profile = renderer
        .last_profile_path()
        .expect("renderer did not record its temporary profile");
    assert!(
        !profile.exists(),
        "profile remains at {}",
        profile.display()
    );
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn chromium_merges_safe_initial_headers_with_browser_defaults() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let (server, requests) = TestServer::spawn_recording([(
        "/",
        TestResponse::html(200, "<html><body>headers</body></html>"),
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let mut request = FetchRequest::browser(server.url("/").as_str()).unwrap();
    request
        .headers
        .insert("x-rscraper-test", "survives".parse().unwrap());

    renderer
        .render(&request, &browser_test_limits())
        .await
        .unwrap();

    let captured = requests.lock().unwrap();
    let initial = captured.first().expect("origin saw no document request");
    assert_eq!(
        initial.headers.get("x-rscraper-test").map(String::as_str),
        Some("survives")
    );
    assert_eq!(
        initial.headers.get("user-agent").map(String::as_str),
        Some(rscraper_core::client::DEFAULT_UA)
    );
    assert!(initial.headers.contains_key("accept"));
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn direct_proxy_validates_each_redirect_and_subresource_connection() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let (destination, destination_requests) = TestServer::spawn_recording([
        (
            "/landing",
            TestResponse::html(
                200,
                r#"<html><body id="state">pending<script src="/app.js"></script></body></html>"#,
            ),
        ),
        (
            "/app.js",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/javascript".into())],
                body: ResponseBody::Fixed(
                    b"document.querySelector('#state').textContent='redirected-script-ok';"
                        .to_vec(),
                ),
            },
        ),
    ])
    .await;
    let (initial, initial_requests) = TestServer::spawn_recording([(
        "/",
        TestResponse::redirect(destination.url("/landing").to_string()),
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let mut request = FetchRequest::browser(initial.url("/").as_str()).unwrap();
    request
        .headers
        .insert("x-initial-only", "present".parse().unwrap());

    let page = renderer
        .render(&request, &browser_test_limits())
        .await
        .unwrap();

    assert_eq!(page.url, destination.url("/landing"));
    assert!(page.html.contains("redirected-script-ok"), "{}", page.html);
    let initial_requests = initial_requests.lock().unwrap();
    assert_eq!(initial_requests.len(), 1);
    assert_eq!(
        initial_requests[0]
            .headers
            .get("x-initial-only")
            .map(String::as_str),
        Some("present")
    );
    let destination_requests = destination_requests.lock().unwrap();
    assert!(destination_requests
        .iter()
        .any(|request| request.target == "/landing"));
    assert!(destination_requests
        .iter()
        .any(|request| request.target == "/app.js"));
    assert!(destination_requests
        .iter()
        .all(|request| !request.headers.contains_key("x-initial-only")));
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn direct_mode_prevents_worker_target_creation_before_worker_code_runs() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let (server, requests) = TestServer::spawn_recording([
        (
            "/",
            TestResponse::html(
                200,
                r##"<html><body id="state">pending<script>
                    try {
                        new Worker("/worker.js");
                        document.querySelector("#state").textContent = "worker-created";
                    } catch (_) {
                        document.querySelector("#state").textContent = "worker-blocked";
                    }
                </script></body></html>"##,
            ),
        ),
        (
            "/worker.js",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/javascript".into())],
                body: ResponseBody::Fixed(
                    b"fetch('/worker-fetch'); new WebSocket('ws://127.0.0.1/socket');".to_vec(),
                ),
            },
        ),
        (
            "/worker-fetch",
            TestResponse::html(200, "worker egress reached"),
        ),
    ])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();

    let page = renderer
        .render(
            &FetchRequest::browser(server.url("/").as_str()).unwrap(),
            &browser_test_limits(),
        )
        .await
        .unwrap();

    assert!(page.html.contains("worker-blocked"), "{}", page.html);
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "worker target reached origin: {requests:?}"
    );
    assert_eq!(requests[0].target, "/");
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn chromium_rejects_unsupported_document_mime_and_oversized_subresources() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let media = TestServer::spawn([(
        "/",
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "video/mp4".into())],
            body: ResponseBody::Fixed(vec![0; 64]),
        },
    )])
    .await;
    let mut limits = browser_test_limits();
    limits.max_body_bytes = 1024;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let error = renderer
        .render(
            &FetchRequest::browser(media.url("/").as_str()).unwrap(),
            &limits,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error:?}");

    let attachment = TestServer::spawn([(
        "/",
        TestResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/html".into()),
                (
                    "Content-Disposition".into(),
                    "attachment; filename=page.html".into(),
                ),
            ],
            body: ResponseBody::Fixed(b"<html>download</html>".to_vec()),
        },
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let error = renderer
        .render(
            &FetchRequest::browser(attachment.url("/").as_str()).unwrap(),
            &limits,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error:?}");

    let large_script = format!("/*{}*/", "x".repeat(2048));
    let server = TestServer::spawn([
        (
            "/",
            TestResponse::html(
                200,
                "<html><body><script src=\"/large.js\"></script>bounded</body></html>",
            ),
        ),
        (
            "/large.js",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/javascript".into())],
                body: ResponseBody::Fixed(large_script.into_bytes()),
            },
        ),
    ])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let error = renderer
        .render(
            &FetchRequest::browser(server.url("/").as_str()).unwrap(),
            &limits,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::BodyLimit { limit: 1024 }),
        "{error:?}"
    );
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn chromium_bounds_declared_chunked_xhr_and_rendered_dom_content() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let mut limits = browser_test_limits();
    limits.max_body_bytes = 1024;

    let declared = TestServer::spawn([(
        "/",
        TestResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/html".into()),
                ("Content-Length".into(), "4096".into()),
            ],
            body: ResponseBody::Fixed(b"<html></html>".to_vec()),
        },
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let error = renderer
        .render(
            &FetchRequest::browser(declared.url("/").as_str()).unwrap(),
            &limits,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::BodyLimit { limit: 1024 }),
        "{error:?}"
    );

    let chunked = TestServer::spawn([(
        "/",
        TestResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/html".into())],
            body: ResponseBody::Chunks(
                (0..8)
                    .map(|_| (Duration::from_millis(10), vec![b'x'; 256]))
                    .collect(),
            ),
        },
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let error = renderer
        .render(
            &FetchRequest::browser(chunked.url("/").as_str()).unwrap(),
            &limits,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::BodyLimit { limit: 1024 }),
        "{error:?}"
    );

    let xhr = TestServer::spawn([
        (
            "/",
            TestResponse::html(
                200,
                r#"<html><body><script>
                    const request = new XMLHttpRequest();
                    request.open("GET", "/large.json", false);
                    try { request.send(); } catch (_) {}
                </script></body></html>"#,
            ),
        ),
        (
            "/large.json",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: ResponseBody::Fixed(vec![b'x'; 2048]),
            },
        ),
    ])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let error = renderer
        .render(
            &FetchRequest::browser(xhr.url("/").as_str()).unwrap(),
            &limits,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::BodyLimit { limit: 1024 }),
        "{error:?}"
    );

    let dom = TestServer::spawn([(
        "/",
        TestResponse::html(
            200,
            r#"<html><body><script>
                document.body.textContent = "x".repeat(4096);
            </script></body></html>"#,
        ),
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let error = renderer
        .render(
            &FetchRequest::browser(dom.url("/").as_str()).unwrap(),
            &limits,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::BodyLimit { limit: 1024 }),
        "{error:?}"
    );
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn chromium_stops_reading_a_stream_after_the_aggregate_budget_is_exceeded() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let written = Arc::new(AtomicUsize::new(0));
    let server_written = Arc::clone(&written);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            if stream.write_all(b"100\r\n").await.is_err()
                || stream.write_all(&vec![b'x'; 256]).await.is_err()
                || stream.write_all(b"\r\n").await.is_err()
            {
                break;
            }
            server_written.fetch_add(1, Ordering::Relaxed);
        }
    });
    let mut limits = browser_test_limits();
    limits.max_body_bytes = 1024;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
    let url = format!("http://{address}/");

    let error = renderer
        .render(&FetchRequest::browser(&url).unwrap(), &limits)
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::BodyLimit { limit: 1024 }),
        "{error:?}"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        written.load(Ordering::Relaxed) < 40,
        "browser downloaded the complete over-budget response"
    );
    server.abort();
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn hostile_main_world_cannot_bypass_the_bounded_dom_serializer() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let server = TestServer::spawn([(
        "/",
        TestResponse::html(
            200,
            r#"<!doctype html><html><body><script>
                globalThis.TextEncoder = class { encode() { return { byteLength: 0 }; } };
                Array.prototype.join = function() { throw new Error("poisoned join"); };
                String.prototype.replaceAll = function() { throw new Error("poisoned replaceAll"); };
                globalThis.Node = {};
                globalThis.Set = class { constructor() { throw new Error("poisoned Set"); } };
                document.body.textContent = "x".repeat(200000);
            </script></body></html>"#,
        ),
    )])
    .await;
    let mut limits = browser_test_limits();
    limits.max_body_bytes = 1024;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();

    let error = renderer
        .render(
            &FetchRequest::browser(server.url("/").as_str()).unwrap(),
            &limits,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, Error::BodyLimit { limit: 1024 }),
        "{error:?}"
    );
    assert!(!renderer.has_active_child());
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn bounded_dom_serializer_preserves_raw_text_and_template_content() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let server = TestServer::spawn([(
        "/",
        TestResponse::html(
            200,
            r#"<!doctype html><html><head>
                <style>.x > .y { content: "<&>"; }</style>
            </head><body>
                <script>if (1 < 2 && 3 > 2) { window.fixture = "<&>"; }</script>
                <template id="fixture"><section data-value="&quot;&amp;">template <b>body</b></section></template>
            </body></html>"#,
        ),
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();

    let page = renderer
        .render(
            &FetchRequest::browser(server.url("/").as_str()).unwrap(),
            &browser_test_limits(),
        )
        .await
        .unwrap();

    assert!(
        page.html
            .contains(r#"<style>.x > .y { content: "<&>"; }</style>"#),
        "{}",
        page.html
    );
    assert!(
        page.html
            .contains(r#"<script>if (1 < 2 && 3 > 2) { window.fixture = "<&>"; }</script>"#),
        "{}",
        page.html
    );
    assert!(
        page.html.contains(
            r#"<template id="fixture"><section data-value="&quot;&amp;">template <b>body</b></section></template>"#
        ),
        "{}",
        page.html
    );
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn browser_rejects_malformed_content_type_parameters() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    for declaration in [
        "text/html; charset",
        "text/html;",
        "text/html; charset=\"unterminated",
        "text/html; charset=utf-8; charset=windows-1252",
    ] {
        let server = TestServer::spawn([(
            "/",
            TestResponse {
                status: 200,
                headers: vec![("Content-Type".into(), declaration.into())],
                body: ResponseBody::Fixed(b"<html><body>secret</body></html>".to_vec()),
            },
        )])
        .await;
        let renderer =
            BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();
        let error = renderer
            .render(
                &FetchRequest::browser(server.url("/").as_str()).unwrap(),
                &browser_test_limits(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Policy(_)),
            "{declaration}: {error:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn browser_preserves_valid_declared_charset_metadata() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let server = TestServer::spawn([(
        "/",
        TestResponse {
            status: 200,
            headers: vec![(
                "Content-Type".into(),
                "text/html; charset=windows-1252".into(),
            )],
            body: ResponseBody::Fixed(b"<html><body><p>caf\xe9</p></body></html>".to_vec()),
        },
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();

    let page = renderer
        .render(
            &FetchRequest::browser(server.url("/").as_str()).unwrap(),
            &browser_test_limits(),
        )
        .await
        .unwrap();

    assert_eq!(
        page.content_type.as_deref(),
        Some("text/html; charset=windows-1252")
    );
    assert!(page.html.contains("café"), "{}", page.html);
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn browser_accepts_semantically_identical_duplicate_content_types() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let server = TestServer::spawn([(
        "/",
        TestResponse {
            status: 200,
            headers: vec![
                (
                    "Content-Type".into(),
                    "text/html; charset=iso-8859-1".into(),
                ),
                (
                    "Content-Type".into(),
                    "TEXT/HTML; charset=windows-1252".into(),
                ),
            ],
            body: ResponseBody::Fixed(b"<html><body>ok</body></html>".to_vec()),
        },
    )])
    .await;
    let renderer =
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap();

    let page = renderer
        .render(
            &FetchRequest::browser(server.url("/").as_str()).unwrap(),
            &browser_test_limits(),
        )
        .await
        .unwrap();

    assert_eq!(
        page.content_type.as_deref(),
        Some("text/html; charset=iso-8859-1")
    );
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn aborting_a_render_promptly_reaps_the_child_and_profile() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let endless = r#"<html><body><script>while (true) {}</script></body></html>"#;
    let server = TestServer::spawn([("/", TestResponse::html(200, endless))]).await;
    let renderer = Arc::new(
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap(),
    );
    let mut limits = browser_test_limits();
    limits.request_timeout = Duration::from_secs(20);
    let task_renderer = Arc::clone(&renderer);
    let request = FetchRequest::browser(server.url("/").as_str()).unwrap();
    let task = tokio::spawn(async move { task_renderer.render(&request, &limits).await });

    tokio::time::timeout(Duration::from_secs(5), async {
        while renderer.active_child_pids().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Chromium child never became active");
    let child_pid = renderer.active_child_pids()[0];
    let profile = renderer.last_profile_path().unwrap();
    task.abort();
    let _ = task.await;

    tokio::time::timeout(Duration::from_secs(2), async {
        while renderer.has_active_child() || profile.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("caller cancellation did not promptly clean the browser session");
    #[cfg(target_os = "linux")]
    assert!(
        !std::path::Path::new(&format!("/proc/{child_pid}")).exists(),
        "Chromium PID {child_pid} still exists after cancellation cleanup"
    );
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn overlapping_renders_never_report_false_inactive_state() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let endless = TestServer::spawn([(
        "/",
        TestResponse::html(200, "<html><script>while (true) {}</script></html>"),
    )])
    .await;
    let quick = TestServer::spawn([(
        "/",
        TestResponse::html(200, "<html><body>quick</body></html>"),
    )])
    .await;
    let renderer = Arc::new(
        BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate)).unwrap(),
    );
    let mut long_limits = browser_test_limits();
    long_limits.request_timeout = Duration::from_secs(5);
    let long_renderer = Arc::clone(&renderer);
    let long_request = FetchRequest::browser(endless.url("/").as_str()).unwrap();
    let long = tokio::spawn(async move { long_renderer.render(&long_request, &long_limits).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while renderer.active_child_pids().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let long_pid = renderer.active_child_pids()[0];
    let long_profile = renderer.active_profile_paths()[0].clone();

    renderer
        .render(
            &FetchRequest::browser(quick.url("/").as_str()).unwrap(),
            &browser_test_limits(),
        )
        .await
        .unwrap();

    assert!(
        renderer.has_active_child(),
        "finishing one render hid another live Chromium child"
    );
    assert!(
        renderer.active_child_pids().contains(&long_pid),
        "finishing one render removed another live Chromium PID"
    );
    long.abort();
    let _ = long.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while renderer.has_active_child() || long_profile.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("overlapping render cancellation did not finish cleanup");
}

#[tokio::test]
#[ignore = "requires a locally installed supported Chromium"]
async fn tor_target_wide_policy_blocks_alternate_targets_and_private_egress() {
    if discover_chromium_executable().is_none() {
        eprintln!("SKIP: no supported Chromium executable is available on PATH");
        return;
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let origin_address = listener.local_addr().unwrap();
    let private_http = format!("http://127.0.0.1:{}/private", origin_address.port());
    let html = format!(
        r##"<!doctype html><html><body id="state">pending
        <iframe src="http://frame.test:{port}/frame"></iframe>
        <script>
            const blocked = [];
            for (const create of [
                () => new Worker("http://worker.test:{port}/worker.js"),
                () => new SharedWorker("http://shared-worker.test:{port}/shared.js"),
                () => new WebSocket("ws://socket.test:{port}/socket"),
                () => new WebTransport("https://transport.test:{port}/transport")
            ]) {{
                try {{ create(); blocked.push(false); }} catch (_) {{ blocked.push(true); }}
            }}
            try {{
                navigator.serviceWorker.register("http://service-worker.test:{port}/sw.js");
                blocked.push(false);
            }} catch (_) {{ blocked.push(true); }}
            blocked.push(window.open("http://popup.test:{port}/popup") === null);
            try {{
                CSS.paintWorklet?.addModule("http://worklet.test:{port}/paint.js");
            }} catch (_) {{}}
            document.querySelector("#state").textContent =
                blocked.every(Boolean) ? "all-alternate-targets-blocked" : JSON.stringify(blocked);
        </script></body></html>"##,
        port = origin_address.port(),
    );
    let private_probe = format!(
        r#"<!doctype html><html><body><script>
            const request = new XMLHttpRequest();
            request.open("GET", {private_http:?}, false);
            try {{ request.send(); }} catch (_) {{}}
        </script></body></html>"#
    );
    let origin_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let html = html.clone();
            let private_probe = private_probe.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let path = std::str::from_utf8(&request)
                    .ok()
                    .and_then(|request| request.lines().next())
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path == "/" {
                    html
                } else if path == "/private-probe" {
                    private_probe
                } else {
                    "alternate-target-egress-reached".to_owned()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let proxy = SocksSpy::forwarding_to(origin_address).await;
    let proxy_url = proxy.proxy_url();
    let target = format!("http://fixture.test:{}/", origin_address.port());
    let renderer = BrowserRenderer::discover(BrowserEgress::TorRequired {
        proxy: proxy_url.clone(),
    })
    .unwrap();
    let mut request = FetchRequest::browser(&target).unwrap();
    request.proxy = Some(proxy_url.clone());

    let page = renderer
        .render(&request, &browser_test_limits())
        .await
        .unwrap();
    assert!(
        page.html.contains("all-alternate-targets-blocked"),
        "{}",
        page.html
    );
    let observed = proxy.requests();
    assert!(
        observed
            .iter()
            .any(|request| request.host == "fixture.test" && request.port == origin_address.port()),
        "SOCKS spy did not receive exact remote-DNS domain request: {observed:?}"
    );
    assert!(
        observed
            .iter()
            .all(|request| request.host == "fixture.test"),
        "unattached background target bypassed the external proxy allowlist: {observed:?}"
    );

    let renderer = BrowserRenderer::discover(BrowserEgress::TorRequired {
        proxy: proxy_url.clone(),
    })
    .unwrap();
    let mut private_request = FetchRequest::browser(&format!(
        "http://fixture.test:{}/private-probe",
        origin_address.port()
    ))
    .unwrap();
    private_request.proxy = Some(proxy_url);
    let error = renderer
        .render(&private_request, &browser_test_limits())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Policy(_)), "{error:?}");
    let observed = proxy.requests();
    assert!(
        observed
            .iter()
            .all(|request| request.host == "fixture.test"),
        "private or background traffic reached the external Tor proxy: {observed:?}"
    );
    origin_task.abort();
}
