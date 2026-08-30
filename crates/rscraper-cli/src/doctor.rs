//! Deterministic local health checks for the shared transport and state.

use crate::context::AppContext;
use crate::cookies::load_platform_cookies;
use rcgen::generate_simple_self_signed;
use rscraper_core::{Error, FetchRequest, Result};
use serde::Serialize;
use std::error::Error as StdError;
use std::fs;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
    pub all_ok: bool,
}

impl DoctorReport {
    pub fn check(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|check| check.name == name)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DoctorOptions {
    pub check_browser: bool,
    pub check_live_services: bool,
}

impl DoctorOptions {
    pub fn standard(check_live_services: bool) -> Self {
        Self {
            check_browser: true,
            check_live_services,
        }
    }

    pub fn offline() -> Self {
        Self {
            check_browser: false,
            check_live_services: false,
        }
    }
}

pub async fn run(
    local_context: &AppContext,
    live_context: Option<&AppContext>,
    options: DoctorOptions,
) -> Result<DoctorReport> {
    if local_context.fetch.policy() != rscraper_core::NetworkPolicy::AllowPrivate {
        return Err(Error::InvalidInput(
            "doctor requires the explicit local diagnostic context".into(),
        ));
    }
    match (options.check_live_services, live_context) {
        (true, Some(context))
            if context.fetch.policy() == rscraper_core::NetworkPolicy::PublicInternet => {}
        (true, _) => {
            return Err(Error::InvalidInput(
                "doctor live checks require a separate PublicInternet context".into(),
            ));
        }
        (false, None) => {}
        (false, Some(_)) => {
            return Err(Error::InvalidInput(
                "doctor received a live context while live checks are disabled".into(),
            ));
        }
    }

    let fixture = LocalHttpFixture::spawn().await?;
    let mut checks = Vec::new();
    checks.push(check_local_request(local_context, fixture.url("/plain")).await);
    checks.push(check_tls_verification(local_context).await?);
    checks.push(check_state_directory(&local_context.config_dir));
    checks.extend(check_cookie_files(&local_context.config_dir));
    checks.push(check_browser_fixture(local_context, &fixture, options.check_browser).await);
    if options.check_live_services {
        checks.push(
            check_optional_live_service(
                live_context.expect("live context was validated before local checks"),
            )
            .await,
        );
    } else {
        checks.push(Check {
            name: "optional external services".into(),
            status: Status::Warn,
            detail: "not checked; pass `doctor --live` to opt in".into(),
            fix: None,
        });
    }
    let all_ok = checks
        .iter()
        .all(|check| !matches!(check.status, Status::Fail));
    Ok(DoctorReport { checks, all_ok })
}

async fn check_local_request(context: &AppContext, url: Url) -> Check {
    match context
        .fetch
        .fetch_request(FetchRequest::request(url.as_str()).expect("fixture URL is valid"))
        .await
    {
        Ok(page) if page.status == 200 && page.html.contains("request-fixture-ok") => Check {
            name: "local request fixture".into(),
            status: Status::Ok,
            detail: "shared FetchClient completed a bounded loopback request".into(),
            fix: None,
        },
        Ok(_) | Err(_) => Check {
            name: "local request fixture".into(),
            status: Status::Fail,
            detail: "shared FetchClient could not complete its local fixture".into(),
            fix: Some("verify local socket permissions and rScrapper transport settings".into()),
        },
    }
}

async fn check_tls_verification(context: &AppContext) -> Result<Check> {
    let fixture = UntrustedTlsFixture::spawn().await?;
    let request = FetchRequest::request(fixture.url().as_str())?;
    let check = match context.fetch.fetch_request(request).await {
        Err(error) if fixture.accepted() && is_unknown_issuer_error(&error) => Check {
            name: "TLS verification".into(),
            status: Status::Ok,
            detail: "shared FetchClient rejected an untrusted local certificate".into(),
            fix: None,
        },
        Err(_) => Check {
            name: "TLS verification".into(),
            status: Status::Fail,
            detail: "local TLS check did not prove the expected certificate rejection".into(),
            fix: Some("verify local socket access and the shared TLS trust policy".into()),
        },
        Ok(_) => Check {
            name: "TLS verification".into(),
            status: Status::Fail,
            detail: "shared FetchClient accepted an untrusted local certificate".into(),
            fix: Some("do not use this build until TLS verification is restored".into()),
        },
    };
    Ok(check)
}

fn is_unknown_issuer_error(error: &Error) -> bool {
    error_source_is_unknown_issuer(error)
}

fn error_source_is_unknown_issuer(error: &(dyn StdError + 'static)) -> bool {
    if matches!(
        error.downcast_ref::<tokio_rustls::rustls::Error>(),
        Some(tokio_rustls::rustls::Error::InvalidCertificate(
            tokio_rustls::rustls::CertificateError::UnknownIssuer
        ))
    ) {
        return true;
    }
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
        if error
            .get_ref()
            .is_some_and(|source| error_source_is_unknown_issuer(source))
        {
            return true;
        }
    }
    error.source().is_some_and(error_source_is_unknown_issuer)
}

fn check_state_directory(path: &std::path::Path) -> Check {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Check {
                name: "local state dir".into(),
                status: Status::Warn,
                detail: "state directory does not exist yet".into(),
                fix: Some("run `rscraper setup <platform>` when cookies are needed".into()),
            }
        }
        Err(_) => {
            return Check {
                name: "local state dir".into(),
                status: Status::Fail,
                detail: "state directory metadata could not be read".into(),
                fix: Some("check owner permissions on RSCRAPER_HOME".into()),
            }
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Check {
            name: "local state dir".into(),
            status: Status::Fail,
            detail: "state path must be a real directory, not a symlink".into(),
            fix: Some("replace RSCRAPER_HOME with an owner-controlled directory".into()),
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Check {
                name: "local state dir".into(),
                status: Status::Fail,
                detail: "state directory permissions are broader than owner-only".into(),
                fix: Some("run `chmod 700 -- <RSCRAPER_HOME>`".into()),
            };
        }
    }
    Check {
        name: "local state dir".into(),
        status: Status::Ok,
        detail: "state directory is private and owner controlled".into(),
        fix: None,
    }
}

fn check_cookie_files(config_dir: &std::path::Path) -> Vec<Check> {
    [
        ("twitter", "twitter.cookies.txt", "https://x.com/"),
        ("reddit", "reddit.cookies.txt", "https://www.reddit.com/"),
        (
            "xiaohongshu",
            "xiaohongshu.cookies.txt",
            "https://www.xiaohongshu.com/",
        ),
        (
            "linkedin",
            "linkedin.cookies.txt",
            "https://www.linkedin.com/",
        ),
    ]
    .into_iter()
    .map(|(platform, file_name, origin)| {
        let path = config_dir.join(file_name);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Check {
                    name: format!("cookies: {platform}"),
                    status: Status::Warn,
                    detail: "not configured; this is optional until the platform is used".into(),
                    fix: Some(format!("run `rscraper setup {platform}`")),
                };
            }
            Ok(_) => {}
            Err(_) => {
                return Check {
                    name: format!("cookies: {platform}"),
                    status: Status::Fail,
                    detail: "cookie file metadata could not be read".into(),
                    fix: Some("check owner permissions on RSCRAPER_HOME".into()),
                };
            }
        }
        let valid = load_platform_cookies(
            &path,
            &Url::parse(origin).expect("static platform origin is valid"),
        )
        .map(|_| ());
        match valid {
            Ok(()) => Check {
                name: format!("cookies: {platform}"),
                status: Status::Ok,
                detail: "cookie file is private, regular, and parseable".into(),
                fix: None,
            },
            Err(_) => Check {
                name: format!("cookies: {platform}"),
                status: Status::Fail,
                detail: "cookie file is insecure or malformed".into(),
                fix: Some(format!(
                    "repair permissions with `chmod 600 -- <cookie-file>` or rerun `rscraper setup {platform}`"
                )),
            },
        }
    })
    .collect()
}

async fn check_browser_fixture(
    context: &AppContext,
    fixture: &LocalHttpFixture,
    enabled: bool,
) -> Check {
    if !enabled {
        return Check {
            name: "browser fixture".into(),
            status: Status::Warn,
            detail: "disabled for this offline check".into(),
            fix: None,
        };
    }
    if context.browser.is_none() {
        return Check {
            name: "browser fixture".into(),
            status: Status::Warn,
            detail: "no supported Chromium installation was found".into(),
            fix: Some("install Chromium or Google Chrome for JavaScript rendering".into()),
        };
    }
    let request = FetchRequest::browser(fixture.url("/browser").as_str())
        .expect("fixture browser URL is valid");
    match context.fetch.fetch_request(request).await {
        Ok(page) if page.html.contains("browser-rendered-ok") => Check {
            name: "browser fixture".into(),
            status: Status::Ok,
            detail: "shared BrowserRenderer executed the local JavaScript fixture".into(),
            fix: None,
        },
        Ok(_) | Err(_) => Check {
            name: "browser fixture".into(),
            status: Status::Fail,
            detail: "installed browser could not render the local fixture".into(),
            fix: Some("check Chromium sandbox and executable permissions".into()),
        },
    }
}

async fn check_optional_live_service(context: &AppContext) -> Check {
    let request = FetchRequest::request("https://example.com/")
        .expect("static optional reachability URL is valid");
    match context.fetch.fetch_request(request).await {
        Ok(page) if (200..400).contains(&page.status) => Check {
            name: "optional external services".into(),
            status: Status::Ok,
            detail: "opt-in external HTTPS reachability succeeded".into(),
            fix: None,
        },
        Ok(_) | Err(_) => Check {
            name: "optional external services".into(),
            status: Status::Warn,
            detail: "opt-in external HTTPS reachability failed".into(),
            fix: Some("check firewall, proxy, or regional service availability".into()),
        },
    }
}

struct LocalHttpFixture {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl LocalHttpFixture {
    async fn spawn() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve_http_fixture(stream));
            }
        });
        Ok(Self { address, task })
    }

    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{}", self.address, path)).expect("local fixture URL is valid")
    }
}

impl Drop for LocalHttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_http_fixture(mut stream: tokio::net::TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 || request.len() > 16 * 1024 {
            return;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let target = String::from_utf8_lossy(&request)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let (status, content_type, body) = if target.starts_with("/plain") {
        (
            200,
            "text/html; charset=utf-8",
            "<main><p>request-fixture-ok</p></main>",
        )
    } else if target.starts_with("/browser") {
        (
            200,
            "text/html; charset=utf-8",
            "<!doctype html><div id='result'>waiting</div><script>document.getElementById('result').textContent='browser-rendered-ok';</script>",
        )
    } else {
        (404, "text/plain; charset=utf-8", "missing")
    };
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

struct UntrustedTlsFixture {
    address: SocketAddr,
    accepted: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl UntrustedTlsFixture {
    async fn spawn() -> Result<Self> {
        let rcgen::CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec!["127.0.0.1".into()]).map_err(|_| {
                Error::Io(std::io::Error::other(
                    "failed to generate local TLS fixture certificate",
                ))
            })?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .map_err(|_| {
                Error::Io(std::io::Error::other(
                    "failed to configure local TLS fixture",
                ))
            })?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let accepted = Arc::new(AtomicBool::new(false));
        let task_accepted = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                task_accepted.store(true, Ordering::SeqCst);
                if let Ok(mut stream) = acceptor.accept(stream).await {
                    let body = "unexpected-tls-success";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            }
        });
        Ok(Self {
            address,
            accepted,
            task,
        })
    }

    fn url(&self) -> Url {
        Url::parse(&format!("https://127.0.0.1:{}/", self.address.port()))
            .expect("local TLS fixture URL is valid")
    }

    fn accepted(&self) -> bool {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for UntrustedTlsFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rscraper_core::{FetchClient, NetworkPolicy, OperationLimits};
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    #[tokio::test]
    async fn arbitrary_transport_rejection_is_not_tls_certificate_proof() {
        let context = AppContext {
            fetch: FetchClient::builder().build().unwrap(),
            browser: None,
            config_dir: TempDir::new().unwrap().path().to_path_buf(),
        };

        let check = check_tls_verification(&context).await.unwrap();

        assert_eq!(check.status, Status::Fail);
    }

    #[tokio::test]
    async fn request_timeout_is_not_tls_certificate_proof() {
        let context = AppContext {
            fetch: FetchClient::builder()
                .policy(NetworkPolicy::AllowPrivate)
                .limits(OperationLimits {
                    request_timeout: Duration::from_nanos(1),
                    ..OperationLimits::default()
                })
                .build()
                .unwrap(),
            browser: None,
            config_dir: TempDir::new().unwrap().path().to_path_buf(),
        };

        let check = check_tls_verification(&context).await.unwrap();

        assert_eq!(check.status, Status::Fail);
    }

    #[tokio::test]
    async fn live_check_refuses_the_private_local_fixture_context() {
        let directory = TempDir::new().unwrap();
        let local = AppContext::try_diagnostic_at(directory.path().to_path_buf()).unwrap();
        let options = DoctorOptions {
            check_browser: false,
            check_live_services: true,
        };

        let error = run(&local, Some(&local), options).await.unwrap_err();

        assert!(matches!(error, Error::InvalidInput(_)));
        assert!(error.to_string().contains("PublicInternet"));
    }

    #[test]
    fn live_public_context_rejects_a_private_redirect_hop_allowed_by_local_fixtures() {
        let directory = TempDir::new().unwrap();
        let local = AppContext::try_diagnostic_at(directory.path().to_path_buf()).unwrap();
        let live = AppContext {
            fetch: FetchClient::builder().build().unwrap(),
            browser: None,
            config_dir: directory.path().to_path_buf(),
        };
        let redirected = FetchRequest::request("http://127.0.0.1/private").unwrap();

        local.fetch.preflight_request(&redirected).unwrap();
        assert!(matches!(
            live.fetch.preflight_request(&redirected),
            Err(Error::Policy(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn broken_cookie_symlink_is_insecure_instead_of_unconfigured() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().unwrap();
        symlink(
            directory.path().join("missing-cookie-target"),
            directory.path().join("twitter.cookies.txt"),
        )
        .unwrap();

        let check = check_cookie_files(directory.path())
            .into_iter()
            .find(|check| check.name == "cookies: twitter")
            .unwrap();

        assert_eq!(check.status, Status::Fail);
    }

    #[tokio::test]
    async fn offline_doctor_uses_local_request_and_tls_fixtures_without_external_network() {
        let directory = TempDir::new().unwrap();
        let state = directory.path().join("state");
        fs::create_dir(&state).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let context = AppContext::try_diagnostic_at(state).unwrap();

        let report = run(&context, None, DoctorOptions::offline()).await.unwrap();

        assert!(report.all_ok);
        assert_eq!(
            report.check("local request fixture").unwrap().status,
            Status::Ok
        );
        assert_eq!(report.check("TLS verification").unwrap().status, Status::Ok);
        assert_eq!(report.check("local state dir").unwrap().status, Status::Ok);
        assert_eq!(
            report.check("browser fixture").unwrap().status,
            Status::Warn
        );
        assert!(report
            .check("browser fixture")
            .unwrap()
            .detail
            .contains("disabled for this offline check"));
    }
}
