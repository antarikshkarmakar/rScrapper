use crate::browser_cdp::{
    validate_caller_headers, BrowserFailure, BrowserPolicy, CdpController, DestinationAllowlist,
    SharedFailure,
};
use crate::client::DEFAULT_UA;
use crate::policy::{address_is_allowed, validate_url, ResolverSource, SystemResolver};
use crate::{Error, FetchRequest, FetchVia, NetworkPolicy, OperationLimits, Page, Result};
use chromiumoxide::async_process::Child;
use chromiumoxide::cdp::browser_protocol::browser::{
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
#[cfg(test)]
use chromiumoxide::cdp::browser_protocol::network::ResourceType;
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CreateIsolatedWorldParams, GetFrameTreeParams,
};
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::{Browser, BrowserConfig};
use futures_util::{FutureExt, StreamExt};
use scraper::{ElementRef, Html, Node, Selector};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::net::{IpAddr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use url::{Host, Url};

const MAX_DOM_BYTES: usize = 5 * 1024 * 1024;
const DOM_RESULT_CHUNK_BYTES: usize = 8 * 1024;
const MAX_PROXY_HEADER_BYTES: usize = 64 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const FORCE_CLOSE_TIMEOUT: Duration = Duration::from_millis(250);
const UNAUTHORIZED_PROXY_DESTINATION: &str =
    "browser proxy destination was not authorized by interception";
const FORBIDDEN_PROXY_RESOLUTION: &str = "browser destination resolved to a forbidden address";

/// Bounded rendering backend registered with [`crate::FetchClientBuilder`].
#[async_trait::async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Render one validated request within the supplied limits.
    async fn render(&self, request: &FetchRequest, limits: &OperationLimits) -> Result<Page>;
}

/// Network egress enforced for an isolated browser session.
#[derive(Debug, Clone)]
pub enum BrowserEgress {
    /// Direct egress still subject to the selected destination policy.
    Direct(NetworkPolicy),
    /// SOCKS5H egress that must remain active for the whole render.
    TorRequired { proxy: Url },
}

#[derive(Debug)]
struct BrowserLifecycleSession {
    profile: PathBuf,
    pid: Option<u32>,
    controller_tasks: Arc<AtomicUsize>,
    cleanup_failed: bool,
    #[cfg(test)]
    proxy_activity: Option<Arc<ProxyActivity>>,
}

#[derive(Debug, Default)]
struct BrowserLifecycle {
    last_profile: Option<PathBuf>,
    next_session_id: u64,
    sessions: HashMap<u64, BrowserLifecycleSession>,
    #[cfg(test)]
    last_proxy_activity: Option<Arc<ProxyActivity>>,
}

impl BrowserLifecycle {
    fn start(
        &mut self,
        profile: PathBuf,
        #[cfg(test)] proxy_activity: Option<Arc<ProxyActivity>>,
    ) -> u64 {
        let session_id = self.next_session_id;
        self.next_session_id = self.next_session_id.wrapping_add(1);
        self.last_profile = Some(profile.clone());
        self.sessions.insert(
            session_id,
            BrowserLifecycleSession {
                profile,
                pid: None,
                controller_tasks: Arc::new(AtomicUsize::new(0)),
                cleanup_failed: false,
                #[cfg(test)]
                proxy_activity,
            },
        );
        session_id
    }

    fn set_pid(&mut self, session_id: u64, pid: Option<u32>) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.pid = pid;
        }
    }

    fn controller_tasks(&self, session_id: u64) -> Option<Arc<AtomicUsize>> {
        self.sessions
            .get(&session_id)
            .map(|session| Arc::clone(&session.controller_tasks))
    }

    fn finish(&mut self, session_id: u64, cleanup_succeeded: bool) {
        #[cfg(test)]
        if let Some(session) = self.sessions.get(&session_id) {
            self.last_proxy_activity = session.proxy_activity.clone();
        }
        if cleanup_succeeded {
            self.sessions.remove(&session_id);
        } else if let Some(session) = self.sessions.get_mut(&session_id) {
            session.cleanup_failed = true;
        }
    }
}

/// Isolated Chromium renderer with owned lifecycle cleanup.
#[derive(Debug)]
pub struct BrowserRenderer {
    executable: PathBuf,
    egress: BrowserEgress,
    lifecycle: Arc<Mutex<BrowserLifecycle>>,
    #[cfg(test)]
    resolver_canary: Option<(String, IpAddr)>,
    #[cfg(test)]
    setup_hook: Option<Arc<BrowserSetupTestHook>>,
    #[cfg(test)]
    session_panic_hook: Option<Arc<BrowserSessionPanicTestHook>>,
    #[cfg(test)]
    launch_env: Vec<(String, String)>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserSetupPhase {
    Writer,
    Reader,
    Event,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct BrowserSetupTestHook {
    pub(crate) phase: BrowserSetupPhase,
    pub(crate) reached: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Semaphore,
    pub(crate) dropped_before_release: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl BrowserSetupTestHook {
    fn new(phase: BrowserSetupPhase) -> Self {
        Self {
            phase,
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            dropped_before_release: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
struct BrowserSessionPanicTestHook {
    reached: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl BrowserSessionPanicTestHook {
    fn new() -> Self {
        Self {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl BrowserRenderer {
    /// Discover a supported executable and validate the requested egress.
    pub fn discover(egress: BrowserEgress) -> Result<Self> {
        if let BrowserEgress::TorRequired { proxy } = &egress {
            validate_tor_proxy(proxy)?;
        }
        let executable = discover_chromium_executable()
            .ok_or_else(|| Error::Browser("no supported Chromium executable was found".into()))?;
        Ok(Self {
            executable,
            egress,
            lifecycle: Arc::new(Mutex::new(BrowserLifecycle::default())),
            #[cfg(test)]
            resolver_canary: None,
            #[cfg(test)]
            setup_hook: None,
            #[cfg(test)]
            session_panic_hook: None,
            #[cfg(test)]
            launch_env: Vec::new(),
        })
    }

    /// Render one request, reaping the child and removing its temporary profile
    /// before returning.
    pub async fn render(&self, request: &FetchRequest, limits: &OperationLimits) -> Result<Page> {
        validate_browser_request(&self.egress, request)?;
        let executable = self.executable.clone();
        let egress = self.egress.clone();
        let lifecycle = Arc::clone(&self.lifecycle);
        let request = request.clone();
        let limits = limits.clone();
        #[cfg(test)]
        let resolver_canary = self.resolver_canary.clone();
        #[cfg(test)]
        let setup_hook = self.setup_hook.clone();
        #[cfg(test)]
        let session_panic_hook = self.session_panic_hook.clone();
        #[cfg(test)]
        let launch_env = self.launch_env.clone();
        let (cancel, cancelled) = oneshot::channel();
        let mut cancel_guard = CancelOnDrop(Some(cancel));
        let result = tokio::spawn(async move {
            render_in_supervisor(
                executable,
                egress,
                lifecycle,
                request,
                limits,
                #[cfg(test)]
                resolver_canary,
                #[cfg(test)]
                setup_hook,
                #[cfg(test)]
                session_panic_hook,
                #[cfg(test)]
                launch_env,
                cancelled,
            )
            .await
        })
        .await
        .map_err(|_| Error::Cancelled)?;
        cancel_guard.0.take();
        result
    }

    #[doc(hidden)]
    pub fn last_profile_path(&self) -> Option<PathBuf> {
        self.lifecycle
            .lock()
            .ok()
            .and_then(|state| state.last_profile.clone())
    }

    #[doc(hidden)]
    pub fn has_active_child(&self) -> bool {
        self.lifecycle
            .lock()
            .map(|state| !state.sessions.is_empty())
            .unwrap_or(true)
    }

    #[doc(hidden)]
    pub fn active_child_pids(&self) -> Vec<u32> {
        self.lifecycle
            .lock()
            .map(|state| {
                state
                    .sessions
                    .values()
                    .filter_map(|session| session.pid)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn active_profile_paths(&self) -> Vec<PathBuf> {
        self.lifecycle
            .lock()
            .map(|state| {
                state
                    .sessions
                    .values()
                    .map(|session| session.profile.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn active_controller_tasks(&self) -> usize {
        self.lifecycle
            .lock()
            .map(|state| {
                state
                    .sessions
                    .values()
                    .map(|session| session.controller_tasks.load(Ordering::SeqCst))
                    .sum()
            })
            .unwrap_or(usize::MAX)
    }

    #[cfg(test)]
    fn proxy_activity_snapshot(&self) -> Option<(bool, usize, bool)> {
        self.lifecycle.lock().ok().and_then(|state| {
            state
                .sessions
                .values()
                .find_map(|session| session.proxy_activity.as_ref())
                .or(state.last_proxy_activity.as_ref())
                .map(|activity| activity.snapshot())
        })
    }

    #[cfg(test)]
    fn inject_session_panic(&mut self) -> Arc<BrowserSessionPanicTestHook> {
        let hook = Arc::new(BrowserSessionPanicTestHook::new());
        self.session_panic_hook = Some(Arc::clone(&hook));
        hook
    }
}

struct CancelOnDrop(Option<oneshot::Sender<()>>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

#[async_trait::async_trait]
impl BrowserBackend for BrowserRenderer {
    async fn render(&self, request: &FetchRequest, limits: &OperationLimits) -> Result<Page> {
        BrowserRenderer::render(self, request, limits).await
    }
}

fn validate_browser_request(egress: &BrowserEgress, request: &FetchRequest) -> Result<()> {
    validate_caller_headers(request)?;
    match egress {
        BrowserEgress::Direct(policy) => {
            validate_url(&request.url, *policy)?;
            if request.proxy.is_some() {
                return Err(Error::Policy(
                    "direct browser egress does not accept a per-request proxy".into(),
                ));
            }
        }
        BrowserEgress::TorRequired { proxy } => {
            validate_url(&request.url, NetworkPolicy::PublicInternet)?;
            if request
                .proxy
                .as_ref()
                .is_some_and(|request_proxy| request_proxy != proxy)
            {
                return Err(Error::Policy(
                    "browser request proxy does not match Tor-required egress".into(),
                ));
            }
        }
    }
    if let Some(restriction) = &request.host_restriction {
        restriction.validate(&request.url)?;
    }
    Ok(())
}

fn validate_tor_proxy(proxy: &Url) -> Result<()> {
    if proxy.scheme() != "socks5h" {
        return Err(Error::InvalidInput(
            "Tor browser proxy must use socks5h".into(),
        ));
    }
    if !proxy.username().is_empty() || proxy.password().is_some() {
        return Err(Error::InvalidInput(
            "Tor browser proxy credentials are not supported".into(),
        ));
    }
    if proxy.query().is_some() || proxy.fragment().is_some() || proxy.path() != "/" {
        return Err(Error::InvalidInput(
            "Tor browser proxy cannot contain a path, query, or fragment".into(),
        ));
    }
    if proxy.port().is_none() || proxy.port() == Some(0) {
        return Err(Error::InvalidInput(
            "Tor browser proxy requires a nonzero explicit port".into(),
        ));
    }
    let Some(address) = tor_proxy_ip(proxy) else {
        return Err(Error::InvalidInput(
            "Tor browser proxy host must be an IP address".into(),
        ));
    };
    if address.is_unspecified() || address.is_multicast() {
        return Err(Error::InvalidInput(
            "Tor browser proxy host must be a usable unicast address".into(),
        ));
    }
    Ok(())
}

fn tor_proxy_ip(proxy: &Url) -> Option<IpAddr> {
    match proxy.host()? {
        Host::Ipv4(address) => Some(IpAddr::V4(address)),
        Host::Ipv6(address) => Some(IpAddr::V6(address)),
        Host::Domain(domain) => domain.parse().ok(),
    }
}

/// Locate a supported Chromium-family executable on `PATH`.
///
/// Unix candidates must be regular files with at least one execute bit.
/// Windows candidates use supported executable base names and `PATHEXT`
/// extension semantics (or the standard executable extensions when `PATHEXT`
/// is absent).
///
/// Discovery performs no download, launch, or network access. Use
/// [`BrowserRenderer::discover`] to validate egress configuration and create a
/// renderer.
pub fn discover_chromium_executable() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let pathext = std::env::var_os("PATHEXT");
    discover_chromium_in_directories(
        std::env::split_paths(&path).filter(|path| !path.as_os_str().is_empty()),
        current_discovery_platform(),
        pathext.as_deref(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscoveryPlatform {
    #[cfg_attr(not(any(test, unix)), allow(dead_code))]
    Unix,
    #[cfg_attr(not(any(test, windows)), allow(dead_code))]
    Windows,
    #[cfg(not(any(unix, windows)))]
    Other,
}

fn current_discovery_platform() -> DiscoveryPlatform {
    #[cfg(unix)]
    {
        DiscoveryPlatform::Unix
    }
    #[cfg(windows)]
    {
        DiscoveryPlatform::Windows
    }
    #[cfg(not(any(unix, windows)))]
    {
        DiscoveryPlatform::Other
    }
}

fn chromium_candidate_names(platform: DiscoveryPlatform, pathext: Option<&OsStr>) -> Vec<OsString> {
    match platform {
        DiscoveryPlatform::Unix => [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        DiscoveryPlatform::Windows => {
            let extensions = windows_executable_extensions(pathext);
            [
                "chrome",
                "chromium",
                "chromium-browser",
                "google-chrome",
                "google-chrome-stable",
            ]
            .into_iter()
            .flat_map(|name| {
                extensions
                    .iter()
                    .map(move |extension| OsString::from(format!("{name}{extension}")))
            })
            .collect()
        }
        #[cfg(not(any(unix, windows)))]
        DiscoveryPlatform::Other => Vec::new(),
    }
}

fn windows_executable_extensions(pathext: Option<&OsStr>) -> Vec<String> {
    const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";
    let Some(raw) = pathext.and_then(OsStr::to_str).or(Some(DEFAULT_PATHEXT)) else {
        return Vec::new();
    };
    let mut extensions = Vec::new();
    for extension in raw.split(';').filter_map(normalize_windows_extension) {
        if !extensions
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&extension))
        {
            extensions.push(extension);
        }
    }
    extensions
}

fn normalize_windows_extension(extension: &str) -> Option<String> {
    let extension = extension.trim();
    if extension.is_empty() {
        return None;
    }
    let extension = if extension.starts_with('.') {
        extension.to_owned()
    } else {
        format!(".{extension}")
    };
    if extension.len() > 16
        || !extension[1..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(extension)
}

fn discover_chromium_in_directories<I>(
    directories: I,
    platform: DiscoveryPlatform,
    pathext: Option<&OsStr>,
) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    let candidates = chromium_candidate_names(platform, pathext);
    for directory in directories {
        for binary in &candidates {
            let candidate = directory.join(binary);
            if chromium_candidate_is_executable(&candidate, platform) {
                return Some(candidate);
            }
        }
    }
    None
}

fn chromium_candidate_is_executable(
    candidate: &std::path::Path,
    platform: DiscoveryPlatform,
) -> bool {
    let Ok(metadata) = candidate.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    match platform {
        DiscoveryPlatform::Unix => unix_metadata_is_executable(&metadata),
        DiscoveryPlatform::Windows => true,
        #[cfg(not(any(unix, windows)))]
        DiscoveryPlatform::Other => false,
    }
}

#[cfg(unix)]
fn unix_metadata_is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn unix_metadata_is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod discovery_tests {
    use super::{chromium_candidate_names, discover_chromium_in_directories, DiscoveryPlatform};
    use std::ffi::OsStr;
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    fn write_unix_candidate(path: &std::path::Path, mode: u32) {
        fs::write(path, b"fixture").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_discovery_accepts_each_supported_executable_name() {
        for (index, name) in [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
        ]
        .into_iter()
        .enumerate()
        {
            let fixture = tempfile::tempdir().unwrap();
            let directory = fixture.path().join(index.to_string());
            fs::create_dir(&directory).unwrap();
            let executable = directory.join(name);
            write_unix_candidate(&executable, 0o700);

            assert_eq!(
                discover_chromium_in_directories([directory], DiscoveryPlatform::Unix, None,),
                Some(executable)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_discovery_rejects_regular_non_executable_candidates() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("first");
        let second = fixture.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let non_executable = first.join("chromium");
        let executable = second.join("google-chrome");
        write_unix_candidate(&non_executable, 0o600);
        write_unix_candidate(&executable, 0o700);

        assert_eq!(
            discover_chromium_in_directories([first, second], DiscoveryPlatform::Unix, None,),
            Some(executable)
        );
    }

    #[test]
    fn windows_discovery_uses_supported_executable_names_and_pathext() {
        let pathext = OsStr::new(".EXE;.CMD");
        let names = chromium_candidate_names(DiscoveryPlatform::Windows, Some(pathext))
            .into_iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "chrome.EXE",
                "chrome.CMD",
                "chromium.EXE",
                "chromium.CMD",
                "chromium-browser.EXE",
                "chromium-browser.CMD",
                "google-chrome.EXE",
                "google-chrome.CMD",
                "google-chrome-stable.EXE",
                "google-chrome-stable.CMD",
            ]
        );

        for name in ["chrome.EXE", "google-chrome.CMD"] {
            let fixture = tempfile::tempdir().unwrap();
            let executable = fixture.path().join(name);
            fs::write(&executable, b"fixture").unwrap();
            assert_eq!(
                discover_chromium_in_directories(
                    [fixture.path().to_path_buf()],
                    DiscoveryPlatform::Windows,
                    Some(pathext),
                ),
                Some(executable)
            );
        }

        let default_names = chromium_candidate_names(DiscoveryPlatform::Windows, None);
        assert!(default_names
            .iter()
            .any(|name| name.to_string_lossy().eq_ignore_ascii_case("chrome.exe")));
    }

    #[test]
    fn windows_discovery_rejects_extensions_missing_from_pathext() {
        let fixture = tempfile::tempdir().unwrap();
        fs::write(fixture.path().join("chrome.BAT"), b"fixture").unwrap();
        assert_eq!(
            discover_chromium_in_directories(
                [fixture.path().to_path_buf()],
                DiscoveryPlatform::Windows,
                Some(OsStr::new(".EXE;.CMD")),
            ),
            None
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn render_in_supervisor(
    executable: PathBuf,
    egress: BrowserEgress,
    lifecycle: Arc<Mutex<BrowserLifecycle>>,
    request: FetchRequest,
    limits: OperationLimits,
    #[cfg(test)] resolver_canary: Option<(String, IpAddr)>,
    #[cfg(test)] setup_hook: Option<Arc<BrowserSetupTestHook>>,
    #[cfg(test)] session_panic_hook: Option<Arc<BrowserSessionPanicTestHook>>,
    #[cfg(test)] launch_env: Vec<(String, String)>,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<Page> {
    let profile = tempfile::Builder::new()
        .prefix("rscraper-browser-")
        .tempdir()
        .map_err(Error::Io)?;
    let policy_failure: SharedFailure = Arc::new(Mutex::new(None));
    let allowlist = DestinationAllowlist::default();
    allowlist.authorize_url(&request.url)?;
    let mut direct_proxy = match egress {
        BrowserEgress::Direct(policy) => Some(
            DirectPolicyProxy::launch(
                policy,
                &limits,
                Arc::clone(&policy_failure),
                allowlist.clone(),
            )
            .await?,
        ),
        BrowserEgress::TorRequired { .. } => None,
    };
    let mut tor_proxy = match &egress {
        BrowserEgress::Direct(_) => None,
        BrowserEgress::TorRequired { proxy } => Some(
            TorPolicyProxy::launch(
                proxy,
                &limits,
                Arc::clone(&policy_failure),
                allowlist.clone(),
            )
            .await?,
        ),
    };
    let proxy =
        match browser_proxy_configuration(&egress, direct_proxy.as_ref(), tor_proxy.as_ref()) {
            Ok(proxy) => proxy,
            Err(error) => {
                if let Some(proxy) = direct_proxy.take() {
                    let _ = proxy.shutdown().await;
                }
                if let Some(proxy) = tor_proxy.take() {
                    let _ = proxy.shutdown().await;
                }
                profile.close().map_err(Error::Io)?;
                return Err(error);
            }
        };
    let config = match build_browser_config(
        executable,
        profile.path(),
        &proxy,
        &limits,
        #[cfg(test)]
        resolver_canary,
        #[cfg(test)]
        &launch_env,
    ) {
        Ok(config) => config,
        Err(error) => {
            if let Some(proxy) = direct_proxy.take() {
                let _ = proxy.shutdown().await;
            }
            if let Some(proxy) = tor_proxy.take() {
                let _ = proxy.shutdown().await;
            }
            profile.close().map_err(Error::Io)?;
            return Err(error);
        }
    };

    #[cfg(test)]
    let proxy_activity = direct_proxy
        .as_ref()
        .map(|proxy| Arc::clone(&proxy.activity))
        .or_else(|| tor_proxy.as_ref().map(|proxy| Arc::clone(&proxy.activity)));
    let (session_id, controller_tasks) = match lifecycle
        .lock()
        .map(|mut state| {
            let session_id = state.start(
                profile.path().to_path_buf(),
                #[cfg(test)]
                proxy_activity,
            );
            let controller_tasks = state
                .controller_tasks(session_id)
                .expect("new lifecycle session has a controller task counter");
            (session_id, controller_tasks)
        })
        .map_err(|_| ())
    {
        Ok(session_id) => session_id,
        Err(_) => {
            if let Some(proxy) = direct_proxy.take() {
                proxy.shutdown().await;
            }
            if let Some(proxy) = tor_proxy.take() {
                proxy.shutdown().await;
            }
            profile.close().map_err(Error::Io)?;
            return Err(Error::Browser("browser lifecycle state is poisoned".into()));
        }
    };

    let mut child = match config.launch() {
        Ok(child) => child,
        Err(_) => {
            let operation = Error::Browser("failed to spawn Chromium".into());
            let cleanup = shutdown_startup(
                None,
                direct_proxy.take(),
                tor_proxy.take(),
                profile,
                Arc::clone(&lifecycle),
                session_id,
            )
            .await;
            return Err(combine_operation_and_cleanup(operation, cleanup));
        }
    };
    if let Ok(mut state) = lifecycle.lock() {
        state.set_pid(session_id, child.as_mut_inner().id());
    }

    let setup_deadline = tokio::time::Instant::now() + browser_setup_timeout(&limits);
    let websocket_address = {
        let mut websocket_discovery = Box::pin(read_devtools_websocket_url(&mut child));
        tokio::select! {
            _ = &mut cancelled => Err(Error::Cancelled),
            discovered = tokio::time::timeout_at(setup_deadline, &mut websocket_discovery) => {
                match discovered {
                    Ok(discovered) => discovered,
                    Err(_) => Err(Error::Timeout { operation: "browser setup" }),
                }
            }
        }
    };
    let websocket_address = match websocket_address {
        Ok(address) => address,
        Err(operation) => {
            let cleanup = shutdown_startup(
                Some(child),
                direct_proxy.take(),
                tor_proxy.take(),
                profile,
                Arc::clone(&lifecycle),
                session_id,
            )
            .await;
            return Err(combine_operation_and_cleanup(operation, cleanup));
        }
    };

    let connected = {
        let mut connection = Box::pin(Browser::connect_with_config(
            websocket_address,
            browser_handler_config(&limits),
        ));
        tokio::select! {
            _ = &mut cancelled => Err(Error::Cancelled),
            connected = tokio::time::timeout_at(setup_deadline, &mut connection) => {
                match connected {
                    Ok(connected) => connected.map_err(|_| {
                        Error::Browser("failed to connect Chromium DevTools".into())
                    }),
                    Err(_) => Err(Error::Timeout { operation: "browser setup" }),
                }
            }
        }
    };
    let (browser, mut handler) = match connected {
        Ok(connected) => connected,
        Err(operation) => {
            let cleanup = shutdown_startup(
                Some(child),
                direct_proxy.take(),
                tor_proxy.take(),
                profile,
                Arc::clone(&lifecycle),
                session_id,
            )
            .await;
            return Err(combine_operation_and_cleanup(operation, cleanup));
        }
    };
    let handler_task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    });
    let mut session = BrowserSession {
        browser,
        child: Some(child),
        handler_task,
        cdp_controller: None,
        direct_proxy,
        tor_proxy,
        profile: Some(profile),
        lifecycle,
        session_id,
    };

    let websocket_address = session.browser.websocket_address().to_owned();
    let controller_websocket = {
        let mut controller_handshake = Box::pin(CdpController::connect_websocket(
            &websocket_address,
            browser_setup_timeout(&limits),
        ));
        tokio::select! {
            _ = &mut cancelled => Err(Error::Cancelled),
            websocket = &mut controller_handshake => websocket,
        }
    };
    let controller_websocket = match controller_websocket {
        Ok(websocket) => websocket,
        Err(operation) => {
            let cleanup = session.shutdown_forcefully().await;
            return Err(combine_operation_and_cleanup(operation, cleanup));
        }
    };

    let mut cancelled_during_controller_setup = false;
    let cdp_controller = {
        let mut controller_setup = Box::pin(CdpController::from_websocket(
            controller_websocket,
            match &egress {
                BrowserEgress::Direct(policy) => BrowserPolicy::Direct(*policy),
                BrowserEgress::TorRequired { .. } => BrowserPolicy::Tor,
            },
            &request,
            limits.request_timeout.min(Duration::from_secs(30)),
            limits.max_redirects,
            limits.max_body_bytes.min(MAX_DOM_BYTES),
            Arc::clone(&policy_failure),
            allowlist,
            controller_tasks,
            #[cfg(test)]
            setup_hook,
        ));
        tokio::select! {
            _ = &mut cancelled => {
                cancelled_during_controller_setup = true;
                controller_setup.await
            }
            controller = &mut controller_setup => controller,
        }
    };
    let cdp_controller = match cdp_controller {
        Ok(controller) => controller,
        Err(error) => {
            let operation = if cancelled_during_controller_setup {
                Error::Cancelled
            } else {
                error
            };
            let cleanup = session.shutdown_forcefully().await;
            return Err(match cleanup {
                Ok(()) => operation,
                Err(cleanup_error) => operation_with_cleanup_failure(operation, cleanup_error),
            });
        }
    };
    session.cdp_controller = Some(cdp_controller);
    if cancelled_during_controller_setup {
        let cleanup = session.shutdown_forcefully().await;
        return Err(match cleanup {
            Ok(()) => Error::Cancelled,
            Err(cleanup_error) => operation_with_cleanup_failure(Error::Cancelled, cleanup_error),
        });
    }

    let session_run = async {
        #[cfg(test)]
        if let Some(hook) = session_panic_hook {
            hook.reached.notify_one();
            let permit = hook
                .release
                .acquire()
                .await
                .expect("injected browser-session panic release was closed");
            permit.forget();
            panic!("injected browser-session panic");
        }
        run_browser_session(&mut session, &egress, &request, &limits, policy_failure).await
    };
    let outcome = tokio::select! {
        _ = &mut cancelled => Err(Error::Cancelled),
        outcome = AssertUnwindSafe(session_run).catch_unwind() => match outcome {
                Ok(outcome) => outcome,
                Err(_) => Err(Error::Browser("browser session panicked".into())),
            },
    };
    let cleanup = session.shutdown().await;
    match (outcome, cleanup) {
        (Ok(page), Ok(())) => Ok(page),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            Err(operation_with_cleanup_failure(error, cleanup_error))
        }
        (Ok(_), Err(error)) => Err(error),
    }
}

fn operation_with_cleanup_failure(operation: Error, cleanup: Error) -> Error {
    Error::Browser(format!(
        "{operation}; browser cleanup also failed: {cleanup}"
    ))
}

fn combine_operation_and_cleanup(operation: Error, cleanup: Result<()>) -> Error {
    match cleanup {
        Ok(()) => operation,
        Err(cleanup_error) => operation_with_cleanup_failure(operation, cleanup_error),
    }
}

fn browser_setup_timeout(limits: &OperationLimits) -> Duration {
    limits.connect_timeout.min(Duration::from_secs(20))
}

fn browser_handler_config(limits: &OperationLimits) -> HandlerConfig {
    HandlerConfig {
        ignore_https_errors: false,
        ignore_invalid_messages: true,
        viewport: Some(Default::default()),
        context_ids: Vec::new(),
        request_timeout: limits.request_timeout.min(Duration::from_secs(30)),
        request_intercept: false,
        cache_enabled: false,
    }
}

async fn read_devtools_websocket_url(child: &mut Child) -> Result<String> {
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Browser("Chromium stderr was not captured".into()))?;
    let mut lines = BufReader::new(stderr.into_inner()).lines();
    while let Some(line) = lines.next_line().await.map_err(Error::Io)? {
        let Some((_, candidate)) = line.rsplit_once("listening on ") else {
            continue;
        };
        let candidate = candidate.trim();
        let parsed = Url::parse(candidate)
            .map_err(|_| Error::Browser("Chromium returned an invalid DevTools URL".into()))?;
        let local_host = match parsed.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        if parsed.scheme() != "ws"
            || !local_host
            || parsed.port().is_none_or(|port| port == 0)
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || !parsed.path().starts_with("/devtools/browser/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Error::Browser(
                "Chromium returned a non-local DevTools URL".into(),
            ));
        }
        return Ok(parsed.into());
    }
    Err(Error::Browser(
        "Chromium exited before reporting its DevTools URL".into(),
    ))
}

async fn reap_owned_child(child: &mut Child) -> bool {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return true;
    }
    let _ = child.as_mut_inner().start_kill();
    matches!(
        tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await,
        Ok(Ok(_))
    )
}

async fn remove_browser_profile(profile: TempDir) -> bool {
    let path = profile.path().to_path_buf();
    if profile.close().is_ok() {
        return true;
    }
    let deadline = tokio::time::Instant::now() + CLEANUP_TIMEOUT;
    loop {
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) if tokio::time::Instant::now() >= deadline => return false,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

async fn shutdown_startup(
    mut child: Option<Child>,
    direct_proxy: Option<DirectPolicyProxy>,
    tor_proxy: Option<TorPolicyProxy>,
    profile: TempDir,
    lifecycle: Arc<Mutex<BrowserLifecycle>>,
    session_id: u64,
) -> Result<()> {
    let reaped = match child.as_mut() {
        Some(child) => reap_owned_child(child).await,
        None => true,
    };
    let direct_proxy_stopped = match direct_proxy {
        Some(proxy) => proxy.shutdown().await,
        None => true,
    };
    let tor_proxy_stopped = match tor_proxy {
        Some(proxy) => proxy.shutdown().await,
        None => true,
    };
    let profile_removed = remove_browser_profile(profile).await;
    let controller_tasks_stopped = lifecycle
        .lock()
        .ok()
        .and_then(|state| state.controller_tasks(session_id))
        .is_some_and(|tasks| tasks.load(Ordering::SeqCst) == 0);
    let cleanup_succeeded = reaped
        && direct_proxy_stopped
        && tor_proxy_stopped
        && controller_tasks_stopped
        && profile_removed;
    if let Ok(mut state) = lifecycle.lock() {
        state.finish(session_id, cleanup_succeeded);
    }
    if cleanup_succeeded {
        Ok(())
    } else {
        Err(Error::Browser(
            "failed to fully reap browser startup resources".into(),
        ))
    }
}

struct BrowserProxyConfiguration {
    server: String,
    resolver_exclusion: String,
}

fn browser_proxy_configuration(
    egress: &BrowserEgress,
    direct_proxy: Option<&DirectPolicyProxy>,
    tor_proxy: Option<&TorPolicyProxy>,
) -> Result<BrowserProxyConfiguration> {
    match egress {
        BrowserEgress::Direct(_) => direct_proxy
            .map(|proxy| BrowserProxyConfiguration {
                server: format!("http://{}", proxy.address),
                resolver_exclusion: proxy.address.ip().to_string(),
            })
            .ok_or_else(|| Error::Policy("direct browser policy proxy is required".into())),
        BrowserEgress::TorRequired { proxy } => {
            validate_tor_proxy(proxy)?;
            tor_proxy
                .map(|proxy| BrowserProxyConfiguration {
                    server: format!("socks5://{}", proxy.address),
                    resolver_exclusion: proxy.address.ip().to_string(),
                })
                .ok_or_else(|| Error::Policy("Tor browser policy proxy is required".into()))
        }
    }
}

fn build_browser_config(
    executable: PathBuf,
    profile: &std::path::Path,
    proxy: &BrowserProxyConfiguration,
    limits: &OperationLimits,
    #[cfg(test)] resolver_canary: Option<(String, IpAddr)>,
    #[cfg(test)] launch_env: &[(String, String)],
) -> Result<BrowserConfig> {
    let launch_timeout = browser_setup_timeout(limits);
    #[cfg(not(test))]
    let resolver_rules = format!("MAP * ~NOTFOUND, EXCLUDE {}", proxy.resolver_exclusion);
    #[cfg(test)]
    let resolver_rules = match resolver_canary {
        Some((host, address)) => format!(
            "MAP {host} {address}, MAP * ~NOTFOUND, EXCLUDE {}",
            proxy.resolver_exclusion
        ),
        None => format!("MAP * ~NOTFOUND, EXCLUDE {}", proxy.resolver_exclusion),
    };
    let builder = BrowserConfig::builder()
        .chrome_executable(executable)
        .user_data_dir(profile)
        .new_headless_mode()
        .incognito()
        .respect_https_errors()
        .disable_cache()
        .launch_timeout(launch_timeout)
        .request_timeout(limits.request_timeout.min(Duration::from_secs(30)))
        .arg(("proxy-server", proxy.server.as_str()))
        .arg(("proxy-bypass-list", "<-loopback>"))
        .arg(("host-resolver-rules", resolver_rules.as_str()))
        .arg("disable-application-cache")
        .arg("disable-component-update")
        .arg("disable-domain-reliability")
        .arg("disable-quic")
        .arg(("disk-cache-size", "0"))
        .arg(("media-cache-size", "0"))
        .arg(("force-webrtc-ip-handling-policy", "disable_non_proxied_udp"))
        .arg(("webrtc-ip-handling-policy", "disable_non_proxied_udp"))
        .arg(("user-agent", DEFAULT_UA));
    #[cfg(test)]
    let builder = builder.envs(launch_env.iter().cloned());
    builder
        .build()
        .map_err(|_| Error::Browser("failed to build Chromium configuration".into()))
}

struct BrowserSession {
    browser: Browser,
    child: Option<Child>,
    handler_task: JoinHandle<()>,
    cdp_controller: Option<CdpController>,
    direct_proxy: Option<DirectPolicyProxy>,
    tor_proxy: Option<TorPolicyProxy>,
    profile: Option<TempDir>,
    lifecycle: Arc<Mutex<BrowserLifecycle>>,
    session_id: u64,
}

impl BrowserSession {
    async fn shutdown(&mut self) -> Result<()> {
        self.shutdown_inner(true).await
    }

    async fn shutdown_forcefully(&mut self) -> Result<()> {
        self.shutdown_inner(false).await
    }

    async fn shutdown_inner(&mut self, graceful: bool) -> Result<()> {
        if let Some(controller) = self.cdp_controller.take() {
            controller.shutdown().await;
        }

        let close_timeout = if graceful {
            CLEANUP_TIMEOUT
        } else {
            FORCE_CLOSE_TIMEOUT
        };
        let _ = tokio::time::timeout(close_timeout, self.browser.close()).await;
        let reaped = match self.child.as_mut() {
            Some(child) if graceful => {
                if matches!(
                    tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await,
                    Ok(Ok(_))
                ) {
                    true
                } else {
                    reap_owned_child(child).await
                }
            }
            Some(child) => reap_owned_child(child).await,
            None => true,
        };
        self.child.take();

        let handler_stopped = if graceful {
            match tokio::time::timeout(CLEANUP_TIMEOUT, &mut self.handler_task).await {
                Ok(_) => true,
                Err(_) => {
                    self.handler_task.abort();
                    tokio::time::timeout(CLEANUP_TIMEOUT, &mut self.handler_task)
                        .await
                        .is_ok()
                }
            }
        } else {
            self.handler_task.abort();
            tokio::time::timeout(CLEANUP_TIMEOUT, &mut self.handler_task)
                .await
                .is_ok()
        };
        let direct_proxy_stopped = match self.direct_proxy.take() {
            Some(proxy) => proxy.shutdown().await,
            None => true,
        };
        let tor_proxy_stopped = match self.tor_proxy.take() {
            Some(proxy) => proxy.shutdown().await,
            None => true,
        };
        let profile_removed = match self.profile.take() {
            Some(profile) => remove_browser_profile(profile).await,
            None => true,
        };
        let controller_tasks_stopped = self
            .lifecycle
            .lock()
            .ok()
            .and_then(|state| state.controller_tasks(self.session_id))
            .is_some_and(|tasks| tasks.load(Ordering::SeqCst) == 0);
        let cleanup_succeeded = reaped
            && handler_stopped
            && direct_proxy_stopped
            && tor_proxy_stopped
            && controller_tasks_stopped
            && profile_removed;
        if let Ok(mut state) = self.lifecycle.lock() {
            state.finish(self.session_id, cleanup_succeeded);
        }
        if !cleanup_succeeded {
            return Err(Error::Browser(
                "failed to fully reap the isolated browser session".into(),
            ));
        }
        Ok(())
    }
}

async fn run_browser_session(
    session: &mut BrowserSession,
    egress: &BrowserEgress,
    request: &FetchRequest,
    limits: &OperationLimits,
    policy_failure: SharedFailure,
) -> Result<Page> {
    session
        .browser
        .execute(SetDownloadBehaviorParams::new(
            SetDownloadBehaviorBehavior::Deny,
        ))
        .await
        .map_err(|_| Error::Browser("failed to disable browser downloads".into()))?;
    let page = session
        .browser
        .new_page("about:blank")
        .await
        .map_err(|_| Error::Browser("failed to create an isolated browser page".into()))?;
    page.execute(AddScriptToEvaluateOnNewDocumentParams {
        source: r#"
            for (const name of [
                "WebSocket", "WebTransport", "EventSource", "RTCPeerConnection",
                "webkitRTCPeerConnection", "Worker", "SharedWorker"
            ]) {
                try {
                    Object.defineProperty(globalThis, name, {
                        configurable: false,
                        value: function () { throw new Error("network primitive disabled"); }
                    });
                } catch (_) {}
            }
            if (navigator.sendBeacon) {
                try {
                    Object.defineProperty(navigator, "sendBeacon", {
                        configurable: false,
                        value: () => false
                    });
                } catch (_) {}
            }
            if (navigator.serviceWorker) {
                try {
                    Object.defineProperty(navigator.serviceWorker, "register", {
                        configurable: false,
                        value: () => { throw new Error("service workers disabled"); }
                    });
                } catch (_) {}
            }
            try {
                Object.defineProperty(globalThis, "open", {
                    configurable: false,
                    value: () => null
                });
            } catch (_) {}
            if (globalThis.Worklet?.prototype?.addModule) {
                try {
                    Object.defineProperty(globalThis.Worklet.prototype, "addModule", {
                        configurable: false,
                        value: () => { throw new Error("worklets disabled"); }
                    });
                } catch (_) {}
            }
        "#
        .into(),
        world_name: None,
        include_command_line_api: None,
        run_immediately: None,
    })
    .await
    .map_err(|_| Error::Browser("failed to install browser network guards".into()))?;

    let navigation_timeout = limits.request_timeout.min(Duration::from_secs(30));
    let navigation = tokio::time::timeout(navigation_timeout, async {
        page.goto(request.url.as_str())
            .await
            .map_err(|_| Error::Browser("browser navigation failed".into()))?;
        page.wait_for_navigation_response()
            .await
            .map_err(|_| Error::Browser("browser navigation did not complete".into()))
    })
    .await;
    let navigation = match navigation {
        Err(_) => {
            return Err(Error::Timeout {
                operation: "browser navigation",
            });
        }
        Ok(Err(error)) => {
            if let Some(failure) = take_policy_failure(&policy_failure) {
                return Err(failure.into_error());
            }
            return Err(error);
        }
        Ok(Ok(navigation)) => navigation,
    };
    if let Some(failure) = take_policy_failure(&policy_failure) {
        return Err(failure.into_error());
    }

    let final_url = page
        .url()
        .await
        .map_err(|_| Error::Browser("failed to read the rendered URL".into()))?
        .ok_or_else(|| Error::Browser("rendered page has no final URL".into()))?;
    let final_url = Url::parse(&final_url)
        .map_err(|_| Error::Browser("rendered page returned an invalid URL".into()))?;
    validate_intercepted_url(egress, &final_url)?;
    if let Some(restriction) = &request.host_restriction {
        restriction.validate(&final_url)?;
    }

    let max_bytes = limits.max_body_bytes.min(MAX_DOM_BYTES);
    let snapshot = collect_bounded_dom(&page, max_bytes, navigation_timeout).await?;
    if let Some(failure) = take_policy_failure(&policy_failure) {
        return Err(failure.into_error());
    }
    let response = navigation
        .as_ref()
        .and_then(|request| request.response.as_ref());
    let status = response
        .and_then(|response| u16::try_from(response.status).ok())
        .unwrap_or(200);
    let content_type = session
        .cdp_controller
        .as_ref()
        .and_then(CdpController::final_content_type);

    Ok(Page {
        url: final_url,
        status,
        content_type,
        html: snapshot,
        via: FetchVia::Browser,
    })
}

#[cfg(test)]
fn intercepted_request_is_allowed(
    egress: &BrowserEgress,
    url: &Url,
    resource_type: &ResourceType,
) -> bool {
    validate_intercepted_url(egress, url).is_ok()
        && matches!(
            resource_type,
            ResourceType::Document
                | ResourceType::Stylesheet
                | ResourceType::Script
                | ResourceType::Xhr
                | ResourceType::Fetch
                | ResourceType::Preflight
        )
}

fn validate_intercepted_url(egress: &BrowserEgress, url: &Url) -> Result<()> {
    match egress {
        BrowserEgress::Direct(policy) => validate_url(url, *policy),
        BrowserEgress::TorRequired { .. } => validate_url(url, NetworkPolicy::PublicInternet),
    }
}

fn take_policy_failure(failure: &SharedFailure) -> Option<BrowserFailure> {
    failure.lock().ok().and_then(|mut failure| failure.take())
}

#[derive(Deserialize)]
struct DomChunk {
    chunk: String,
    done: bool,
}

async fn collect_bounded_dom(
    page: &chromiumoxide::Page,
    max_bytes: usize,
    timeout: Duration,
) -> Result<String> {
    tokio::time::timeout(timeout, async {
        let frame_tree = page
            .execute(GetFrameTreeParams::default())
            .await
            .map_err(|_| Error::Browser("failed to identify the rendered frame".into()))?;
        let isolated_world = page
            .execute(CreateIsolatedWorldParams {
                frame_id: frame_tree.result.frame_tree.frame.id.clone(),
                world_name: Some("rscraper-bounded-dom".into()),
                grant_univeral_access: Some(false),
            })
            .await
            .map_err(|_| Error::Browser("failed to isolate rendered content collection".into()))?;
        let context_id = isolated_world.result.execution_context_id;

        let initialization = EvaluateParams::builder()
            .expression(BOUNDED_DOM_SERIALIZER)
            .context_id(context_id)
            .return_by_value(true)
            .build()
            .map_err(|_| {
                Error::Browser("failed to configure rendered content collection".into())
            })?;
        let initialized = page.execute(initialization).await.map_err(|_| {
            Error::Browser("failed to configure rendered content collection".into())
        })?;
        if initialized.result.exception_details.is_some()
            || initialized.result.result.value.as_ref() != Some(&serde_json::Value::Bool(true))
        {
            return Err(Error::Browser(
                "rendered content collector did not initialize".into(),
            ));
        }

        let mut html = String::new();
        loop {
            let pull = EvaluateParams::builder()
                .expression("globalThis.__rscraperBoundedDom.pull()")
                .context_id(context_id)
                .return_by_value(true)
                .build()
                .map_err(|_| Error::Browser("failed to read rendered content".into()))?;
            let response = page
                .execute(pull)
                .await
                .map_err(|_| Error::Browser("failed to read rendered content".into()))?;
            if response.result.exception_details.is_some() {
                return Err(Error::Browser("rendered content collection failed".into()));
            }
            let value = response
                .result
                .result
                .value
                .ok_or_else(|| Error::Browser("rendered content had no value".into()))?;
            let chunk: DomChunk = serde_json::from_value(value)
                .map_err(|_| Error::Browser("rendered content had an invalid shape".into()))?;
            if append_dom_chunk(&mut html, chunk, max_bytes)? {
                break;
            }
        }
        if html.len() > max_bytes {
            return Err(Error::BodyLimit { limit: max_bytes });
        }
        Ok(html)
    })
    .await
    .map_err(|_| Error::Timeout {
        operation: "browser content",
    })?
}

fn append_dom_chunk(html: &mut String, chunk: DomChunk, max_bytes: usize) -> Result<bool> {
    let chunk_bytes = chunk.chunk.len();
    if chunk_bytes > DOM_RESULT_CHUNK_BYTES {
        return Err(Error::Browser(
            "rendered content exceeded the transfer chunk bound".into(),
        ));
    }
    let next_size = html
        .len()
        .checked_add(chunk_bytes)
        .ok_or(Error::BodyLimit { limit: max_bytes })?;
    if next_size > max_bytes {
        return Err(Error::BodyLimit { limit: max_bytes });
    }
    html.push_str(&chunk.chunk);
    Ok(chunk.done)
}

const BOUNDED_DOM_SERIALIZER: &str = r#"(() => {
    const encoder = new TextEncoder();
    const join = Function.call.bind(Array.prototype.join);
    const replace = Function.call.bind(String.prototype.replace);
    const slice = Function.call.bind(String.prototype.slice);
    const charCodeAt = Function.call.bind(String.prototype.charCodeAt);
    const chunkLimit = 8192;
    const sourceSlice = 1024;
    const stack = [{ kind: "node", node: document, raw: false }];
    let pendingFragment = null;

    const isVoid = tag => tag === "area" || tag === "base" || tag === "br" ||
        tag === "col" || tag === "embed" || tag === "hr" || tag === "img" ||
        tag === "input" || tag === "link" || tag === "meta" || tag === "param" ||
        tag === "source" || tag === "track" || tag === "wbr";
    const pushLiteral = (value, escaping) => stack.push({
        kind: "literal", value, offset: 0, escaping
    });
    const escapedSlice = (value, offset, escaping) => {
        let end = Math.min(offset + sourceSlice, value.length);
        if (end < value.length) {
            const code = charCodeAt(value, end - 1);
            if (code >= 0xd800 && code <= 0xdbff) end -= 1;
        }
        let fragment = slice(value, offset, end);
        if (escaping !== "raw") {
            fragment = replace(fragment, /&/g, "&amp;");
            if (escaping === "text") {
                fragment = replace(fragment, /</g, "&lt;");
                fragment = replace(fragment, />/g, "&gt;");
            } else {
                fragment = replace(fragment, /\"/g, "&quot;");
            }
        }
        return { fragment, end };
    };
    const nextFragment = () => {
        while (stack.length) {
            const item = stack.pop();
            if (item.kind === "literal") {
                if (item.offset >= item.value.length) continue;
                const next = escapedSlice(item.value, item.offset, item.escaping);
                item.offset = next.end;
                if (item.offset < item.value.length) stack.push(item);
                return next.fragment;
            }
            if (item.kind === "open") {
                if (item.phase === 0) {
                    item.phase = 1;
                    stack.push(item);
                    pushLiteral(item.node.localName, "raw");
                    pushLiteral("<", "raw");
                    continue;
                }
                const attributes = item.node.attributes;
                if (item.index < attributes.length) {
                    const attribute = attributes.item(item.index);
                    item.index += 1;
                    stack.push(item);
                    pushLiteral("\"", "raw");
                    pushLiteral(attribute.value, "attribute");
                    pushLiteral("=\"", "raw");
                    pushLiteral(attribute.name, "raw");
                    pushLiteral(" ", "raw");
                    continue;
                }
                pushLiteral(">", "raw");
                continue;
            }

            const node = item.node;
            if (node.nextSibling) {
                stack.push({ kind: "node", node: node.nextSibling, raw: item.raw });
            }
            if (node.nodeType === 9 || node.nodeType === 11) {
                if (node.firstChild) {
                    stack.push({ kind: "node", node: node.firstChild, raw: false });
                }
            } else if (node.nodeType === 10) {
                pushLiteral(">", "raw");
                if (node.systemId) {
                    pushLiteral("\"", "raw");
                    pushLiteral(node.systemId, "raw");
                    pushLiteral(node.publicId ? " \"" : " SYSTEM \"", "raw");
                }
                if (node.publicId) {
                    pushLiteral("\"", "raw");
                    pushLiteral(node.publicId, "raw");
                    pushLiteral(" PUBLIC \"", "raw");
                }
                pushLiteral(node.name, "raw");
                pushLiteral("<!DOCTYPE ", "raw");
            } else if (node.nodeType === 1) {
                const tag = node.localName;
                if (!isVoid(tag)) {
                    pushLiteral(">", "raw");
                    pushLiteral(tag, "raw");
                    pushLiteral("</", "raw");
                    const first = tag === "template" && node.content
                        ? node.content.firstChild
                        : node.firstChild;
                    if (first) {
                        stack.push({
                            kind: "node", node: first,
                            raw: tag === "script" || tag === "style"
                        });
                    }
                }
                stack.push({ kind: "open", node, index: 0, phase: 0 });
            } else if (node.nodeType === 3) {
                pushLiteral(node.data, item.raw ? "raw" : "text");
            } else if (node.nodeType === 8) {
                pushLiteral("-->", "raw");
                pushLiteral(node.data, "raw");
                pushLiteral("<!--", "raw");
            }
        }
        return null;
    };

    globalThis.__rscraperBoundedDom = {
        pull() {
            const pieces = [];
            let bytes = 0;
            while (true) {
                const fragment = pendingFragment === null ? nextFragment() : pendingFragment;
                pendingFragment = null;
                if (fragment === null) return { chunk: join(pieces, ""), done: true };
                const length = encoder.encode(fragment).byteLength;
                if (length > chunkLimit) throw new Error("serializer fragment exceeded bound");
                if (length > chunkLimit - bytes) {
                    pendingFragment = fragment;
                    return { chunk: join(pieces, ""), done: false };
                }
                pieces.push(fragment);
                bytes += length;
                if (bytes === chunkLimit) {
                    return { chunk: join(pieces, ""), done: false };
                }
            }
        }
    };
    return true;
})()"#;

struct DirectPolicyProxy {
    address: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
    #[cfg(test)]
    activity: Arc<ProxyActivity>,
}

#[cfg(test)]
#[derive(Debug)]
struct ProxyActivity {
    listener_open: AtomicBool,
    connections: AtomicUsize,
    terminal: AtomicBool,
}

#[cfg(test)]
impl ProxyActivity {
    fn listening() -> Arc<Self> {
        Arc::new(Self {
            listener_open: AtomicBool::new(true),
            connections: AtomicUsize::new(0),
            terminal: AtomicBool::new(false),
        })
    }

    fn snapshot(&self) -> (bool, usize, bool) {
        (
            self.listener_open.load(Ordering::SeqCst),
            self.connections.load(Ordering::SeqCst),
            self.terminal.load(Ordering::SeqCst),
        )
    }
}

#[cfg(test)]
struct ProxyConnectionActivity(Arc<ProxyActivity>);

#[cfg(test)]
impl ProxyConnectionActivity {
    fn start(activity: Arc<ProxyActivity>) -> Self {
        activity.connections.fetch_add(1, Ordering::SeqCst);
        Self(activity)
    }
}

#[cfg(test)]
impl Drop for ProxyConnectionActivity {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

impl DirectPolicyProxy {
    async fn launch(
        policy: NetworkPolicy,
        limits: &OperationLimits,
        policy_failure: SharedFailure,
        allowlist: DestinationAllowlist,
    ) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let limits = limits.clone();
        #[cfg(test)]
        let activity = ProxyActivity::listening();
        #[cfg(test)]
        let task_activity = Arc::clone(&activity);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let limits = limits.clone();
                            let policy_failure = Arc::clone(&policy_failure);
                            let allowlist = allowlist.clone();
                            #[cfg(test)]
                            let connection_activity =
                                ProxyConnectionActivity::start(Arc::clone(&task_activity));
                            connections.spawn(async move {
                                #[cfg(test)]
                                let _connection_activity = connection_activity;
                                if let Err(Error::Policy(message)) = handle_policy_proxy(
                                    stream,
                                    policy,
                                    &limits,
                                    &allowlist,
                                ).await {
                                    if message != FORBIDDEN_PROXY_RESOLUTION {
                                        return;
                                    }
                                    if let Ok(mut failure) = policy_failure.lock() {
                                        if failure.is_none() {
                                            *failure = Some(BrowserFailure::Policy(format!(
                                                "browser destination violated network policy: {message}"
                                            )));
                                        }
                                    }
                                }
                            });
                        }
                        Err(_) => break,
                    },
                    completed = connections.join_next(), if !connections.is_empty() => {
                        let _ = completed;
                    }
                }
            }
            drop(listener);
            #[cfg(test)]
            task_activity.listener_open.store(false, Ordering::SeqCst);
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            #[cfg(test)]
            task_activity.terminal.store(true, Ordering::SeqCst);
        });
        Ok(Self {
            address,
            shutdown,
            task,
            #[cfg(test)]
            activity,
        })
    }

    async fn shutdown(self) -> bool {
        let _ = self.shutdown.send(());
        self.task.await.is_ok()
    }
}

struct TorPolicyProxy {
    address: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
    #[cfg(test)]
    activity: Arc<ProxyActivity>,
}

impl TorPolicyProxy {
    async fn launch(
        upstream_proxy: &Url,
        limits: &OperationLimits,
        policy_failure: SharedFailure,
        allowlist: DestinationAllowlist,
    ) -> Result<Self> {
        validate_tor_proxy(upstream_proxy)?;
        let upstream_address = SocketAddr::new(
            tor_proxy_ip(upstream_proxy).expect("validated Tor proxy has an IP host"),
            upstream_proxy
                .port()
                .expect("validated Tor proxy has an explicit port"),
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let limits = limits.clone();
        #[cfg(test)]
        let activity = ProxyActivity::listening();
        #[cfg(test)]
        let task_activity = Arc::clone(&activity);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let limits = limits.clone();
                            let allowlist = allowlist.clone();
                            let policy_failure = Arc::clone(&policy_failure);
                            #[cfg(test)]
                            let connection_activity =
                                ProxyConnectionActivity::start(Arc::clone(&task_activity));
                            connections.spawn(async move {
                                #[cfg(test)]
                                let _connection_activity = connection_activity;
                                if let Err(Error::Policy(message)) = handle_tor_policy_proxy(
                                    stream,
                                    upstream_address,
                                    &limits,
                                    &allowlist,
                                ).await {
                                    if message != UNAUTHORIZED_PROXY_DESTINATION {
                                        if let Ok(mut failure) = policy_failure.lock() {
                                            if failure.is_none() {
                                                *failure = Some(BrowserFailure::Policy(message));
                                            }
                                        }
                                    }
                                }
                            });
                        }
                        Err(_) => break,
                    },
                    completed = connections.join_next(), if !connections.is_empty() => {
                        let _ = completed;
                    }
                }
            }
            drop(listener);
            #[cfg(test)]
            task_activity.listener_open.store(false, Ordering::SeqCst);
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            #[cfg(test)]
            task_activity.terminal.store(true, Ordering::SeqCst);
        });
        Ok(Self {
            address,
            shutdown,
            task,
            #[cfg(test)]
            activity,
        })
    }

    async fn shutdown(self) -> bool {
        let _ = self.shutdown.send(());
        self.task.await.is_ok()
    }
}

async fn handle_tor_policy_proxy(
    mut client: TcpStream,
    upstream_address: SocketAddr,
    limits: &OperationLimits,
    allowlist: &DestinationAllowlist,
) -> Result<()> {
    tokio::time::timeout(limits.request_timeout, async {
        let version = client.read_u8().await?;
        let method_count = client.read_u8().await?;
        if version != 5 || method_count == 0 {
            return Err(Error::Policy("invalid browser SOCKS5 greeting".into()));
        }
        let mut methods = vec![0; usize::from(method_count)];
        client.read_exact(&mut methods).await?;
        if !methods.contains(&0) {
            client.write_all(&[5, 0xff]).await?;
            return Err(Error::Policy(
                "browser SOCKS5 client did not offer no-auth".into(),
            ));
        }
        client.write_all(&[5, 0]).await?;

        let mut prefix = [0_u8; 4];
        client.read_exact(&mut prefix).await?;
        if prefix[..3] != [5, 1, 0] {
            send_socks_failure(&mut client, 7).await?;
            return Err(Error::Policy(
                "browser SOCKS5 request was not CONNECT".into(),
            ));
        }
        let (host, mut encoded_address) = read_socks_address(&mut client, prefix[3]).await?;
        let port = client.read_u16().await?;
        if port == 0 {
            send_socks_failure(&mut client, 8).await?;
            return Err(Error::Policy(
                "browser SOCKS5 destination has port zero".into(),
            ));
        }
        validate_socks_destination(&host, port)?;
        if !allowlist.allows(&host, port) {
            send_socks_failure(&mut client, 2).await?;
            return Err(Error::Policy(UNAUTHORIZED_PROXY_DESTINATION.into()));
        }

        let mut upstream =
            tokio::time::timeout(limits.connect_timeout, TcpStream::connect(upstream_address))
                .await
                .map_err(|_| Error::Timeout {
                    operation: "Tor proxy connection",
                })??;
        upstream.write_all(&[5, 1, 0]).await?;
        let mut greeting_response = [0_u8; 2];
        upstream.read_exact(&mut greeting_response).await?;
        if greeting_response != [5, 0] {
            send_socks_failure(&mut client, 1).await?;
            return Err(Error::Browser(
                "configured Tor proxy rejected no-auth SOCKS5".into(),
            ));
        }
        let mut request = vec![5, 1, 0, prefix[3]];
        request.append(&mut encoded_address);
        request.extend_from_slice(&port.to_be_bytes());
        upstream.write_all(&request).await?;
        let reply = read_socks_reply(&mut upstream).await?;
        client.write_all(&reply).await?;
        if reply.get(1) != Some(&0) {
            return Ok(());
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        Ok(())
    })
    .await
    .map_err(|_| Error::Timeout {
        operation: "browser Tor policy proxy",
    })?
}

async fn read_socks_address(
    stream: &mut TcpStream,
    address_type: u8,
) -> Result<(Host<String>, Vec<u8>)> {
    match address_type {
        1 => {
            let mut octets = [0; 4];
            stream.read_exact(&mut octets).await?;
            Ok((Host::Ipv4(octets.into()), octets.into_iter().collect()))
        }
        3 => {
            let length = stream.read_u8().await?;
            if length == 0 {
                return Err(Error::Policy(
                    "browser SOCKS5 domain cannot be empty".into(),
                ));
            }
            let mut domain = vec![0; usize::from(length)];
            stream.read_exact(&mut domain).await?;
            let domain = String::from_utf8(domain)
                .map_err(|_| Error::Policy("browser SOCKS5 domain is not UTF-8".into()))?;
            let mut encoded = vec![length];
            encoded.extend_from_slice(domain.as_bytes());
            Ok((Host::Domain(domain), encoded))
        }
        4 => {
            let mut octets = [0; 16];
            stream.read_exact(&mut octets).await?;
            Ok((Host::Ipv6(octets.into()), octets.into_iter().collect()))
        }
        _ => Err(Error::Policy(
            "browser SOCKS5 request has an invalid address type".into(),
        )),
    }
}

fn validate_socks_destination(host: &Host<String>, port: u16) -> Result<()> {
    match host {
        Host::Ipv4(address) => {
            if !address_is_allowed(NetworkPolicy::PublicInternet, IpAddr::V4(*address)) {
                return Err(Error::Policy(
                    "Tor browser destination is a forbidden IPv4 address".into(),
                ));
            }
        }
        Host::Ipv6(address) => {
            if !address_is_allowed(NetworkPolicy::PublicInternet, IpAddr::V6(*address)) {
                return Err(Error::Policy(
                    "Tor browser destination is a forbidden IPv6 address".into(),
                ));
            }
        }
        Host::Domain(domain) => {
            let host = if domain.contains(':') {
                format!("[{domain}]")
            } else {
                domain.clone()
            };
            let url = Url::parse(&format!("http://{host}:{port}/"))
                .map_err(|_| Error::Policy("Tor browser destination host is invalid".into()))?;
            validate_url(&url, NetworkPolicy::PublicInternet)?;
        }
    }
    Ok(())
}

async fn read_socks_reply(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix).await?;
    if prefix[0] != 5 || prefix[2] != 0 {
        return Err(Error::Browser(
            "configured Tor proxy returned an invalid SOCKS5 reply".into(),
        ));
    }
    let mut reply = prefix.to_vec();
    match prefix[3] {
        1 => {
            let mut tail = [0; 6];
            stream.read_exact(&mut tail).await?;
            reply.extend_from_slice(&tail);
        }
        3 => {
            let length = stream.read_u8().await?;
            reply.push(length);
            let mut tail = vec![0; usize::from(length) + 2];
            stream.read_exact(&mut tail).await?;
            reply.extend_from_slice(&tail);
        }
        4 => {
            let mut tail = [0; 18];
            stream.read_exact(&mut tail).await?;
            reply.extend_from_slice(&tail);
        }
        _ => {
            return Err(Error::Browser(
                "configured Tor proxy returned an invalid address type".into(),
            ));
        }
    }
    Ok(reply)
}

async fn send_socks_failure(stream: &mut TcpStream, code: u8) -> Result<()> {
    stream.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    Ok(())
}

async fn handle_policy_proxy(
    mut client: TcpStream,
    policy: NetworkPolicy,
    limits: &OperationLimits,
    allowlist: &DestinationAllowlist,
) -> Result<()> {
    let request = read_proxy_request(&mut client, limits.request_timeout).await?;
    let head_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
        .ok_or_else(|| Error::Policy("invalid browser proxy request".into()))?;
    let head = std::str::from_utf8(&request[..head_end])
        .map_err(|_| Error::Policy("invalid browser proxy request".into()))?;
    let first_line = head
        .lines()
        .next()
        .ok_or_else(|| Error::Policy("invalid browser proxy request".into()))?;
    let mut parts = first_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| Error::Policy("invalid browser proxy request".into()))?;
    let target = parts
        .next()
        .ok_or_else(|| Error::Policy("invalid browser proxy request".into()))?;
    let version = parts
        .next()
        .ok_or_else(|| Error::Policy("invalid browser proxy request".into()))?;
    if parts.next().is_some() {
        return Err(Error::Policy("invalid browser proxy request".into()));
    }
    if version != "HTTP/1.1" {
        return Err(Error::Policy(
            "browser proxy requires HTTP/1.1 request framing".into(),
        ));
    }

    if method.eq_ignore_ascii_case("CONNECT") {
        if request.len() != head_end {
            return Err(Error::Policy(
                "browser proxy CONNECT cannot contain pipelined bytes".into(),
            ));
        }
        validate_proxy_headers(head.lines().skip(1), None)?;
        let (host, port) = parse_proxy_authority(target)?;
        if !allowlist.allows(&host, port) {
            return Err(Error::Policy(UNAUTHORIZED_PROXY_DESTINATION.into()));
        }
        let mut upstream = connect_policy_destination(&host, port, policy, limits).await?;
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        return Ok(());
    }

    if request.len() != head_end {
        return Err(Error::Policy(
            "browser proxy request cannot contain a body or pipelined bytes".into(),
        ));
    }
    if !matches!(method, "GET" | "HEAD" | "OPTIONS") {
        return Err(Error::Policy(
            "browser proxy request method is not supported".into(),
        ));
    }

    let url =
        Url::parse(target).map_err(|_| Error::Policy("invalid browser proxy target".into()))?;
    if url.scheme() != "http" {
        return Err(Error::Policy(
            "browser HTTP proxy requires an http absolute target".into(),
        ));
    }
    validate_url(&url, policy)?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| Error::Policy("browser proxy target requires a port".into()))?;
    let host = url
        .host()
        .ok_or_else(|| Error::Policy("browser proxy target requires a host".into()))?
        .to_owned();
    if !allowlist.allows(&host, port) {
        return Err(Error::Policy(UNAUTHORIZED_PROXY_DESTINATION.into()));
    }
    let mut upstream = connect_policy_destination(&host, port, policy, limits).await?;
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let target = match url.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    let authority = proxy_url_authority(&url)?;
    let forwarded_headers = validate_proxy_headers(head.lines().skip(1), Some(&authority))?;
    let mut rewritten = format!("{method} {target} {version}\r\n");
    rewritten.push_str("Host: ");
    rewritten.push_str(&authority);
    rewritten.push_str("\r\n");
    for (name, value) in forwarded_headers {
        rewritten.push_str(&name);
        rewritten.push_str(": ");
        rewritten.push_str(&value);
        rewritten.push_str("\r\n");
    }
    rewritten.push_str("Connection: close\r\n\r\n");
    upstream.write_all(rewritten.as_bytes()).await?;
    let _ = tokio::io::copy(&mut upstream, &mut client).await;
    Ok(())
}

fn proxy_url_authority(url: &Url) -> Result<String> {
    let host = match url
        .host()
        .ok_or_else(|| Error::Policy("browser proxy target requires a host".into()))?
    {
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => format!("[{address}]"),
        Host::Domain(domain) => domain.to_owned(),
    };
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn validate_proxy_headers<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    expected_host: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let mut forwarded = Vec::new();
    let mut host = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(Error::Policy(
                "browser proxy request contains a folded header".into(),
            ));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::Policy("browser proxy request has an invalid header".into()))?;
        let parsed_name =
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                Error::Policy("browser proxy request has an invalid header name".into())
            })?;
        let parsed_value = reqwest::header::HeaderValue::from_str(value.trim()).map_err(|_| {
            Error::Policy("browser proxy request has an invalid header value".into())
        })?;
        let lower = parsed_name.as_str();
        if lower == "host" {
            if host
                .replace(parsed_value.to_str().unwrap_or_default().to_owned())
                .is_some()
            {
                return Err(Error::Policy(
                    "browser proxy request contains duplicate Host headers".into(),
                ));
            }
            continue;
        }
        if matches!(
            lower,
            "connection"
                | "content-length"
                | "transfer-encoding"
                | "proxy-authorization"
                | "proxy-authenticate"
        ) {
            return Err(Error::Policy(
                "browser proxy request contains a restricted header".into(),
            ));
        }
        if matches!(
            lower,
            "proxy-connection" | "keep-alive" | "te" | "trailer" | "upgrade"
        ) {
            continue;
        }
        forwarded.push((
            parsed_name.as_str().to_owned(),
            parsed_value.to_str().unwrap_or_default().to_owned(),
        ));
    }

    if let Some(expected) = expected_host {
        let host = host.ok_or_else(|| {
            Error::Policy("browser proxy request requires one Host header".into())
        })?;
        if !host.eq_ignore_ascii_case(expected) {
            return Err(Error::Policy(
                "browser proxy Host does not match the target authority".into(),
            ));
        }
    }
    Ok(forwarded)
}

async fn read_proxy_request(stream: &mut TcpStream, timeout: Duration) -> Result<Vec<u8>> {
    tokio::time::timeout(timeout, async {
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Err(Error::Policy("incomplete browser proxy request".into()));
            }
            request.extend_from_slice(&buffer[..read]);
            if request.len() > MAX_PROXY_HEADER_BYTES {
                return Err(Error::BodyLimit {
                    limit: MAX_PROXY_HEADER_BYTES,
                });
            }
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(request);
            }
        }
    })
    .await
    .map_err(|_| Error::Timeout {
        operation: "browser proxy request",
    })?
}

fn parse_proxy_authority(authority: &str) -> Result<(Host<String>, u16)> {
    let url = Url::parse(&format!("http://{authority}/"))
        .map_err(|_| Error::Policy("invalid browser proxy authority".into()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Policy(
            "browser proxy authority cannot contain credentials".into(),
        ));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(Error::Policy(
            "browser proxy authority cannot contain a path, query, or fragment".into(),
        ));
    }
    let host = url
        .host()
        .ok_or_else(|| Error::Policy("browser proxy authority requires a host".into()))?
        .to_owned();
    let port = url
        .port()
        .ok_or_else(|| Error::Policy("browser proxy authority requires a port".into()))?;
    Ok((host, port))
}

async fn connect_policy_destination(
    host: &Host<String>,
    port: u16,
    policy: NetworkPolicy,
    limits: &OperationLimits,
) -> Result<TcpStream> {
    let addresses = match host {
        Host::Ipv4(address) => vec![IpAddr::V4(*address)],
        Host::Ipv6(address) => vec![IpAddr::V6(*address)],
        Host::Domain(domain) => SystemResolver
            .resolve(domain.clone())
            .await
            .map_err(|_| Error::Dns("browser destination resolution failed".into()))?
            .into_iter()
            .map(|address| address.ip())
            .collect(),
    };
    if addresses.is_empty() {
        return Err(Error::Dns(
            "browser destination resolution returned no addresses".into(),
        ));
    }
    if addresses
        .iter()
        .any(|address| !address_is_allowed(policy, *address))
    {
        return Err(Error::Policy(FORBIDDEN_PROXY_RESOLUTION.into()));
    }

    for address in addresses {
        let socket = SocketAddr::new(address, port);
        if let Ok(Ok(stream)) =
            tokio::time::timeout(limits.connect_timeout, TcpStream::connect(socket)).await
        {
            return Ok(stream);
        }
    }
    Err(Error::Dns("browser destination connection failed".into()))
}

pub fn looks_like_javascript_shell(html: &str) -> bool {
    if html.trim().is_empty() {
        return true;
    }

    let document = Html::parse_document(html);
    let body_selector = Selector::parse("body").expect("hard-coded body selector is valid");
    let title_selector = Selector::parse("title").expect("hard-coded title selector is valid");
    let paragraph_selector = Selector::parse("p").expect("hard-coded paragraph selector is valid");
    let body = document
        .select(&body_selector)
        .next()
        .unwrap_or_else(|| document.root_element());
    let visible = visible_text(body);
    let word_count = visible.split_whitespace().count();
    if word_count == 0 {
        return true;
    }

    let lower_html = html.to_ascii_lowercase();
    const CHALLENGE_HINTS: [&str; 8] = [
        "enable javascript",
        "please enable js",
        "checking your browser",
        "verify you are human",
        "are you a robot",
        "just a moment",
        "_cf_chl_opt",
        "cf-chl-",
    ];
    if word_count < 120 && CHALLENGE_HINTS.iter().any(|hint| lower_html.contains(hint)) {
        return true;
    }

    let has_title = document
        .select(&title_selector)
        .any(|element| !visible_text(element).trim().is_empty());
    let has_paragraph = document
        .select(&paragraph_selector)
        .any(|element| !visible_text(element).trim().is_empty());
    if has_title && has_paragraph {
        return false;
    }

    word_count < 3
}

fn visible_text(root: ElementRef<'_>) -> String {
    root.descendants()
        .filter_map(|node| match node.value() {
            Node::Text(text)
                if !node
                    .ancestors()
                    .filter_map(ElementRef::wrap)
                    .any(|element| {
                        matches!(
                            element.value().name(),
                            "script" | "style" | "template" | "svg"
                        )
                    }) =>
            {
                Some(text.trim())
            }
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod policy_tests {
    use super::{
        handle_policy_proxy, intercepted_request_is_allowed, read_socks_address,
        validate_tor_proxy, BrowserEgress, BrowserLifecycle, BrowserRenderer, BrowserSetupPhase,
        BrowserSetupTestHook,
    };
    use crate::browser_cdp::DestinationAllowlist;
    use crate::{Error, FetchRequest, NetworkPolicy, OperationLimits};
    use async_tungstenite::tokio::accept_async;
    use chromiumoxide::cdp::browser_protocol::network::ResourceType;
    use futures_util::StreamExt;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use url::Url;

    async fn spawn_setup_fixture_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = vec![0_u8; 4096];
                    let _ = stream.read(&mut request).await;
                    let body = b"<html><body>setup fixture</body></html>";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });
        (address, task)
    }

    async fn spawn_stalled_devtools_server(
        healthy_upgrades: usize,
    ) -> (
        SocketAddr,
        Arc<tokio::sync::Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let stalled = Arc::new(tokio::sync::Notify::new());
        let stalled_signal = Arc::clone(&stalled);
        let task = tokio::spawn(async move {
            let mut upgraded_connections = tokio::task::JoinSet::new();
            for _ in 0..healthy_upgrades {
                let (stream, _) = listener.accept().await.unwrap();
                let websocket = accept_async(stream).await.unwrap();
                upgraded_connections.spawn(async move {
                    let mut websocket = websocket;
                    while let Some(Ok(message)) = websocket.next().await {
                        let Ok(text) = message.into_text() else {
                            continue;
                        };
                        let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        let Some(id) = request.get("id").and_then(serde_json::Value::as_u64) else {
                            continue;
                        };
                        let response = serde_json::json!({"id": id, "result": {}});
                        if websocket
                            .send(async_tungstenite::tungstenite::Message::Text(
                                response.to_string().into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
            let (_stalled_stream, _) = listener.accept().await.unwrap();
            stalled_signal.notify_one();
            std::future::pending::<()>().await;
        });
        (address, stalled, task)
    }

    fn stalled_upgrade_renderer(
        address: SocketAddr,
        fixture_dir: &std::path::Path,
    ) -> Arc<BrowserRenderer> {
        let executable = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/chromium_stalled_upgrade.sh");
        Arc::new(BrowserRenderer {
            executable,
            egress: BrowserEgress::Direct(crate::NetworkPolicy::AllowPrivate),
            lifecycle: Arc::new(Mutex::new(BrowserLifecycle::default())),
            resolver_canary: None,
            setup_hook: None,
            session_panic_hook: None,
            launch_env: vec![
                (
                    "RSCRAPER_STALLED_DEVTOOLS_URL".into(),
                    format!("ws://{address}/devtools/browser/stalled"),
                ),
                (
                    "RSCRAPER_STALLED_UPGRADE_DIR".into(),
                    fixture_dir.to_string_lossy().into_owned(),
                ),
            ],
        })
    }

    async fn wait_for_file(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fixture process did not report its PID");
    }

    async fn wait_for_proxy_connection(renderer: &BrowserRenderer, expected_connections: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if renderer
                    .proxy_activity_snapshot()
                    .is_some_and(|(_, connections, _)| connections == expected_connections)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("policy proxy did not acquire the expected owned connection");
    }

    #[cfg(target_os = "linux")]
    struct ProcessGuard(u32);

    #[cfg(target_os = "linux")]
    impl Drop for ProcessGuard {
        fn drop(&mut self) {
            if std::path::Path::new(&format!("/proc/{}", self.0)).exists() {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &self.0.to_string()])
                    .status();
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancellation_during_initial_devtools_upgrade_reaps_owned_child_within_setup_bound() {
        let fixture_dir = tempfile::tempdir().unwrap();
        let (devtools_address, stalled, devtools_server) = spawn_stalled_devtools_server(0).await;
        let renderer = stalled_upgrade_renderer(devtools_address, fixture_dir.path());
        let (target_address, target_server) = spawn_setup_fixture_server().await;
        let request = FetchRequest::browser(&format!("http://{target_address}/")).unwrap();
        let limits = OperationLimits {
            connect_timeout: Duration::from_millis(250),
            request_timeout: Duration::from_secs(2),
            ..OperationLimits::default()
        };
        let task_renderer = Arc::clone(&renderer);
        let task = tokio::spawn(async move { task_renderer.render(&request, &limits).await });

        wait_for_file(&fixture_dir.path().join("child.pid")).await;
        tokio::time::timeout(Duration::from_secs(5), stalled.notified())
            .await
            .expect("initial DevTools connection did not reach the stalled upgrade");
        let child_pid: u32 = std::fs::read_to_string(fixture_dir.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let guard = ProcessGuard(child_pid);
        let proxy_address: SocketAddr =
            std::fs::read_to_string(fixture_dir.path().join("proxy.address"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        let profile = renderer.active_profile_paths()[0].clone();
        let pid_was_owned = renderer.active_child_pids().contains(&child_pid);
        let (listener_open, _, terminal) = renderer
            .proxy_activity_snapshot()
            .expect("the active browser session must own its policy proxy");
        assert!(listener_open);
        assert!(!terminal);
        assert!(renderer.has_active_child());
        assert!(profile.exists());
        let proxy_connection = TcpStream::connect(proxy_address).await.unwrap();
        wait_for_proxy_connection(&renderer, 1).await;

        task.abort();
        let _ = task.await;
        let cleaned = tokio::time::timeout(Duration::from_millis(1500), async {
            while renderer.has_active_child() || profile.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        if cleaned.is_err() {
            devtools_server.abort();
            tokio::time::timeout(Duration::from_secs(5), async {
                while renderer.has_active_child() || profile.exists() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("RED cleanup failed after releasing the stalled upgrade");
            panic!("cancelled initial DevTools upgrade exceeded the configured setup bound");
        }

        assert!(
            pid_was_owned,
            "the spawned child PID was not lifecycle-owned"
        );
        assert_eq!(renderer.active_controller_tasks(), 0);
        assert!(!std::path::Path::new(&format!("/proc/{child_pid}")).exists());
        let proxy_activity = renderer
            .proxy_activity_snapshot()
            .expect("cleanup must retain a terminal policy-proxy snapshot");
        assert_eq!(proxy_activity, (false, 0, true));
        drop(proxy_connection);
        std::mem::forget(guard);
        devtools_server.abort();
        target_server.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn initial_devtools_upgrade_deadline_reaps_owned_child_and_full_session() {
        let fixture_dir = tempfile::tempdir().unwrap();
        let (devtools_address, stalled, devtools_server) = spawn_stalled_devtools_server(0).await;
        let renderer = stalled_upgrade_renderer(devtools_address, fixture_dir.path());
        let (target_address, target_server) = spawn_setup_fixture_server().await;
        let request = FetchRequest::browser(&format!("http://{target_address}/")).unwrap();
        let limits = OperationLimits {
            connect_timeout: Duration::from_millis(250),
            request_timeout: Duration::from_secs(2),
            ..OperationLimits::default()
        };
        let task_renderer = Arc::clone(&renderer);
        let mut task = tokio::spawn(async move { task_renderer.render(&request, &limits).await });

        wait_for_file(&fixture_dir.path().join("child.pid")).await;
        tokio::time::timeout(Duration::from_secs(5), stalled.notified())
            .await
            .expect("initial DevTools connection did not reach the stalled upgrade");
        let child_pid: u32 = std::fs::read_to_string(fixture_dir.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let guard = ProcessGuard(child_pid);
        let proxy_address: SocketAddr =
            std::fs::read_to_string(fixture_dir.path().join("proxy.address"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        let profile = renderer.active_profile_paths()[0].clone();
        assert!(renderer.active_child_pids().contains(&child_pid));
        let proxy_connection = TcpStream::connect(proxy_address).await.unwrap();
        wait_for_proxy_connection(&renderer, 1).await;

        let render_result = match tokio::time::timeout(Duration::from_millis(1500), &mut task).await
        {
            Ok(result) => result.unwrap(),
            Err(_) => {
                task.abort();
                let _ = task.await;
                devtools_server.abort();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while renderer.has_active_child() || profile.exists() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("RED cleanup failed after releasing the initial stalled upgrade");
                panic!("initial DevTools upgrade exceeded the configured setup deadline");
            }
        };
        assert!(matches!(
            render_result,
            Err(Error::Timeout {
                operation: "browser setup"
            })
        ));
        assert!(!renderer.has_active_child());
        assert_eq!(renderer.active_controller_tasks(), 0);
        assert!(!profile.exists());
        assert!(!std::path::Path::new(&format!("/proc/{child_pid}")).exists());
        assert_eq!(renderer.proxy_activity_snapshot(), Some((false, 0, true)));
        drop(proxy_connection);
        std::mem::forget(guard);
        devtools_server.abort();
        target_server.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn raw_controller_upgrade_deadline_stops_before_tasks_and_cleans_session() {
        let fixture_dir = tempfile::tempdir().unwrap();
        let (devtools_address, stalled, devtools_server) = spawn_stalled_devtools_server(1).await;
        let renderer = stalled_upgrade_renderer(devtools_address, fixture_dir.path());
        let (target_address, target_server) = spawn_setup_fixture_server().await;
        let request = FetchRequest::browser(&format!("http://{target_address}/")).unwrap();
        let limits = OperationLimits {
            connect_timeout: Duration::from_millis(250),
            request_timeout: Duration::from_secs(2),
            ..OperationLimits::default()
        };
        let task_renderer = Arc::clone(&renderer);
        let mut task = tokio::spawn(async move { task_renderer.render(&request, &limits).await });

        wait_for_file(&fixture_dir.path().join("child.pid")).await;
        tokio::time::timeout(Duration::from_secs(5), stalled.notified())
            .await
            .expect("raw controller did not reach the stalled WebSocket upgrade");
        let child_pid: u32 = std::fs::read_to_string(fixture_dir.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let guard = ProcessGuard(child_pid);
        let proxy_address: SocketAddr =
            std::fs::read_to_string(fixture_dir.path().join("proxy.address"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        let profile = renderer.active_profile_paths()[0].clone();
        assert!(renderer.active_child_pids().contains(&child_pid));
        assert_eq!(renderer.active_controller_tasks(), 0);
        let proxy_connection = TcpStream::connect(proxy_address).await.unwrap();
        wait_for_proxy_connection(&renderer, 1).await;

        let render_result = match tokio::time::timeout(Duration::from_millis(1500), &mut task).await
        {
            Ok(result) => result.unwrap(),
            Err(_) => {
                task.abort();
                let _ = task.await;
                devtools_server.abort();
                tokio::time::timeout(Duration::from_secs(8), async {
                    while renderer.has_active_child() || profile.exists() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("RED cleanup failed after releasing the stalled raw upgrade");
                panic!("raw-controller upgrade exceeded the configured setup deadline");
            }
        };
        assert!(matches!(
            render_result,
            Err(Error::Timeout {
                operation: "browser policy controller setup"
            })
        ));
        if renderer.has_active_child() || profile.exists() {
            devtools_server.abort();
            panic!("raw-controller deadline returned before full session cleanup");
        }

        assert_eq!(renderer.active_controller_tasks(), 0);
        assert!(!std::path::Path::new(&format!("/proc/{child_pid}")).exists());
        assert_eq!(renderer.proxy_activity_snapshot(), Some((false, 0, true)));
        drop(proxy_connection);
        std::mem::forget(guard);
        devtools_server.abort();
        target_server.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn session_panic_cleans_every_owned_lifecycle_resource() {
        let fixture_dir = tempfile::tempdir().unwrap();
        let (devtools_address, _stalled, devtools_server) = spawn_stalled_devtools_server(2).await;
        let mut renderer = stalled_upgrade_renderer(devtools_address, fixture_dir.path());
        let panic_hook = Arc::get_mut(&mut renderer).unwrap().inject_session_panic();
        let (target_address, target_server) = spawn_setup_fixture_server().await;
        let request = FetchRequest::browser(&format!("http://{target_address}/")).unwrap();
        let limits = OperationLimits {
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(3),
            ..OperationLimits::default()
        };
        let task_renderer = Arc::clone(&renderer);
        let task = tokio::spawn(async move { task_renderer.render(&request, &limits).await });

        wait_for_file(&fixture_dir.path().join("child.pid")).await;
        tokio::time::timeout(Duration::from_secs(5), panic_hook.reached.notified())
            .await
            .expect("browser session did not reach the injected panic seam");
        let child_pid: u32 = std::fs::read_to_string(fixture_dir.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let guard = ProcessGuard(child_pid);
        let proxy_address: SocketAddr =
            std::fs::read_to_string(fixture_dir.path().join("proxy.address"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        let profile = renderer.active_profile_paths()[0].clone();
        let proxy_connection = TcpStream::connect(proxy_address).await.unwrap();
        wait_for_proxy_connection(&renderer, 1).await;

        panic_hook.release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("panic cleanup exceeded its fixed lifecycle bound")
            .unwrap();
        assert!(matches!(
            result,
            Err(Error::Browser(message)) if message.contains("browser session panicked")
        ));
        assert!(!renderer.has_active_child());
        assert_eq!(renderer.active_controller_tasks(), 0);
        assert!(!profile.exists());
        assert!(!std::path::Path::new(&format!("/proc/{child_pid}")).exists());
        assert_eq!(renderer.proxy_activity_snapshot(), Some((false, 0, true)));
        drop(proxy_connection);
        std::mem::forget(guard);
        devtools_server.abort();
        target_server.abort();
    }

    #[tokio::test]
    #[ignore = "requires a locally installed supported Chromium"]
    async fn cancellation_at_each_raw_cdp_task_spawn_keeps_setup_owned_until_cleanup() {
        let Some(executable) = super::discover_chromium_executable() else {
            eprintln!("SKIP: no supported Chromium executable is available on PATH");
            return;
        };
        let (address, server) = spawn_setup_fixture_server().await;
        for phase in [
            BrowserSetupPhase::Writer,
            BrowserSetupPhase::Reader,
            BrowserSetupPhase::Event,
        ] {
            let hook = Arc::new(BrowserSetupTestHook::new(phase));
            let renderer = Arc::new(BrowserRenderer {
                executable: executable.clone(),
                egress: BrowserEgress::Direct(crate::NetworkPolicy::AllowPrivate),
                lifecycle: Arc::new(Mutex::new(BrowserLifecycle::default())),
                resolver_canary: None,
                setup_hook: Some(Arc::clone(&hook)),
                session_panic_hook: None,
                launch_env: Vec::new(),
            });
            let reached = hook.reached.notified();
            let task_renderer = Arc::clone(&renderer);
            let request = FetchRequest::browser(&format!("http://{address}/")).unwrap();
            let limits = OperationLimits {
                request_timeout: Duration::from_secs(10),
                ..OperationLimits::default()
            };
            let task = tokio::spawn(async move { task_renderer.render(&request, &limits).await });
            tokio::time::timeout(Duration::from_secs(5), reached)
                .await
                .expect("raw CDP setup did not reach the injected barrier");
            let pid = renderer.active_child_pids()[0];
            let profile = renderer.active_profile_paths()[0].clone();

            task.abort();
            let _ = task.await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                !hook.dropped_before_release.load(Ordering::SeqCst),
                "{phase:?} construction future was dropped on cancellation"
            );
            assert!(
                renderer.has_active_child(),
                "{phase:?} reported inactive before setup cleanup was released"
            );
            hook.release.add_permits(1);

            let cleanup = tokio::time::timeout(Duration::from_secs(5), async {
                while renderer.has_active_child()
                    || renderer.active_controller_tasks() != 0
                    || profile.exists()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(
                cleanup.is_ok(),
                "{phase:?} setup-time cancellation did not finish owned cleanup; active={}, tasks={}, profile={}",
                renderer.has_active_child(),
                renderer.active_controller_tasks(),
                profile.exists()
            );
            #[cfg(target_os = "linux")]
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        }
        server.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a locally installed supported Chromium"]
    async fn cancellation_after_process_spawn_reaps_explicitly_owned_real_pid() {
        let Some(executable) = super::discover_chromium_executable() else {
            eprintln!("SKIP: no supported Chromium executable is available on PATH");
            return;
        };
        let barrier = tempfile::tempdir().unwrap();
        let wrapper = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/chromium_launch_barrier.sh");
        let renderer = Arc::new(BrowserRenderer {
            executable: wrapper,
            egress: BrowserEgress::Direct(crate::NetworkPolicy::AllowPrivate),
            lifecycle: Arc::new(Mutex::new(BrowserLifecycle::default())),
            resolver_canary: None,
            setup_hook: None,
            session_panic_hook: None,
            launch_env: vec![
                (
                    "RSCRAPER_REAL_CHROMIUM".into(),
                    executable.to_string_lossy().into_owned(),
                ),
                (
                    "RSCRAPER_LAUNCH_BARRIER_DIR".into(),
                    barrier.path().to_string_lossy().into_owned(),
                ),
            ],
        });
        let (address, server) = spawn_setup_fixture_server().await;
        let request = FetchRequest::browser(&format!("http://{address}/")).unwrap();
        let limits = OperationLimits {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
            ..OperationLimits::default()
        };
        let task_renderer = Arc::clone(&renderer);
        let task = tokio::spawn(async move { task_renderer.render(&request, &limits).await });
        let reached = barrier.path().join("reached");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !reached.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Chromium process did not reach the post-spawn launch barrier");
        let chromium_pid: u32 = std::fs::read_to_string(barrier.path().join("chromium.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        struct ProcessGuard(u32);
        impl Drop for ProcessGuard {
            fn drop(&mut self) {
                if std::path::Path::new(&format!("/proc/{}", self.0)).exists() {
                    let _ = std::process::Command::new("kill")
                        .args(["-KILL", &self.0.to_string()])
                        .status();
                }
            }
        }
        let guard = ProcessGuard(chromium_pid);
        let filter_pid: u32 = std::fs::read_to_string(barrier.path().join("filter.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let profile = renderer.active_profile_paths()[0].clone();
        assert!(renderer.active_child_pids().contains(&chromium_pid));
        assert!(renderer.has_active_child());
        assert!(std::path::Path::new(&format!("/proc/{chromium_pid}")).exists());

        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while renderer.has_active_child()
                || profile.exists()
                || std::path::Path::new(&format!("/proc/{filter_pid}")).exists()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("launch cancellation did not kill and wait the explicitly owned child");
        assert_eq!(renderer.active_controller_tasks(), 0);
        assert!(!std::path::Path::new(&format!("/proc/{chromium_pid}")).exists());
        std::mem::forget(guard);
        server.abort();
    }

    #[test]
    fn failed_cleanup_remains_visible_in_per_session_accounting() {
        let mut lifecycle = BrowserLifecycle::default();
        let session_id = lifecycle.start("/tmp/rscraper-profile-fixture".into(), None);
        lifecycle.set_pid(session_id, Some(41));

        lifecycle.finish(session_id, false);

        let session = lifecycle.sessions.get(&session_id).unwrap();
        assert_eq!(session.pid, Some(41));
        assert!(session.cleanup_failed);
        assert_eq!(
            session.profile,
            std::path::PathBuf::from("/tmp/rscraper-profile-fixture")
        );
    }

    #[test]
    fn operation_and_cleanup_failures_are_both_reported() {
        let error = super::operation_with_cleanup_failure(
            Error::Policy("operation fixture".into()),
            Error::Browser("cleanup fixture".into()),
        );
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("operation fixture"), "{diagnostic}");
        assert!(diagnostic.contains("cleanup fixture"), "{diagnostic}");
    }

    #[test]
    fn rust_rejects_a_browser_dom_chunk_that_lies_about_the_bound() {
        let mut html = "prefix".to_owned();
        let error = super::append_dom_chunk(
            &mut html,
            super::DomChunk {
                chunk: "x".repeat(1024),
                done: true,
            },
            64,
        )
        .unwrap_err();

        assert!(matches!(error, Error::BodyLimit { limit: 64 }));
        assert_eq!(html, "prefix");
    }

    #[test]
    fn direct_interception_rejects_private_destinations_and_media() {
        let public = Url::parse("https://example.com/app.js").unwrap();
        let private = Url::parse("http://127.0.0.1/private.js").unwrap();
        let egress = BrowserEgress::Direct(NetworkPolicy::PublicInternet);

        assert!(intercepted_request_is_allowed(
            &egress,
            &public,
            &ResourceType::Script
        ));
        assert!(!intercepted_request_is_allowed(
            &egress,
            &private,
            &ResourceType::Script
        ));
        assert!(!intercepted_request_is_allowed(
            &egress,
            &public,
            &ResourceType::Media
        ));
    }

    #[test]
    fn tor_interception_allows_onion_documents_but_rejects_private_literals() {
        let egress = BrowserEgress::TorRequired {
            proxy: Url::parse("socks5h://127.0.0.1:9050/").unwrap(),
        };
        assert!(intercepted_request_is_allowed(
            &egress,
            &Url::parse("http://examplehiddenservice.onion/").unwrap(),
            &ResourceType::Document
        ));
        assert!(!intercepted_request_is_allowed(
            &egress,
            &Url::parse("http://127.0.0.1/").unwrap(),
            &ResourceType::Document
        ));
    }

    #[test]
    fn tor_endpoint_validation_rejects_unusable_literals_without_launching() {
        for value in [
            "socks5h://127.0.0.1:0/",
            "socks5h://0.0.0.0:9050/",
            "socks5h://[::]:9050/",
            "socks5h://224.0.0.1:9050/",
            "socks5h://[ff02::1]:9050/",
            "socks5h://127.0.0.1/",
            "socks5h://user@127.0.0.1:9050/",
            "socks5h://127.0.0.1:9050/path",
            "socks5h://127.0.0.1:9050/?query=yes",
            "socks5h://127.0.0.1:9050/#fragment",
        ] {
            let proxy = Url::parse(value).unwrap();
            assert!(validate_tor_proxy(&proxy).is_err(), "accepted {value}");
        }

        for value in [
            "socks5h://127.0.0.1:9050/",
            "socks5h://10.0.0.1:9050/",
            "socks5h://[::1]:9050/",
            "socks5h://[fd00::1]:9050/",
        ] {
            let proxy = Url::parse(value).unwrap();
            assert!(validate_tor_proxy(&proxy).is_ok(), "rejected {value}");
        }
    }

    #[tokio::test]
    #[ignore = "requires a locally installed supported Chromium"]
    async fn tor_rejection_triggers_neither_local_resolution_nor_direct_fallback() {
        if super::discover_chromium_executable().is_none() {
            eprintln!("SKIP: no supported Chromium executable is available on PATH");
            return;
        }
        let direct_canary = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let target_port = direct_canary.local_addr().unwrap().port();
        let socks = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let socks_address = socks.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let spy_observed = Arc::clone(&observed);
        let spy = tokio::spawn(async move {
            while let Ok((mut stream, _)) = socks.accept().await {
                let observed = Arc::clone(&spy_observed);
                tokio::spawn(async move {
                    let version = stream.read_u8().await.ok()?;
                    let method_count = stream.read_u8().await.ok()?;
                    if version != 5 || method_count == 0 {
                        return None;
                    }
                    let mut methods = vec![0; usize::from(method_count)];
                    stream.read_exact(&mut methods).await.ok()?;
                    if !methods.contains(&0) {
                        return None;
                    }
                    stream.write_all(&[5, 0]).await.ok()?;
                    let mut prefix = [0; 4];
                    stream.read_exact(&mut prefix).await.ok()?;
                    if prefix[..3] != [5, 1, 0] {
                        return None;
                    }
                    let (host, _) = read_socks_address(&mut stream, prefix[3]).await.ok()?;
                    let port = stream.read_u16().await.ok()?;
                    observed.lock().ok()?.push((prefix[3], host, port));
                    stream
                        .write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0])
                        .await
                        .ok()?;
                    Some(())
                });
            }
        });
        let proxy = Url::parse(&format!("socks5h://{socks_address}/")).unwrap();
        let mut renderer = BrowserRenderer::discover(BrowserEgress::TorRequired {
            proxy: proxy.clone(),
        })
        .unwrap();
        renderer.resolver_canary = Some((
            "public-fixture.test".into(),
            "127.0.0.1".parse::<IpAddr>().unwrap(),
        ));
        let mut request =
            FetchRequest::browser(&format!("http://public-fixture.test:{target_port}/")).unwrap();
        request.proxy = Some(proxy);

        let error = renderer
            .render(&request, &proxy_test_limits())
            .await
            .unwrap_err();

        assert!(
            matches!(error, Error::Browser(_) | Error::Timeout { .. }),
            "{error:?}"
        );
        {
            let observed = observed.lock().unwrap();
            assert!(!observed.is_empty(), "SOCKS spy saw no target request");
            assert!(
                observed.iter().all(|(address_type, host, port)| {
                    *address_type == 3
                        && matches!(host, url::Host::Domain(domain) if domain == "public-fixture.test")
                        && *port == target_port
                }),
                "local resolution changed the SOCKS target: {observed:?}"
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), direct_canary.accept())
                .await
                .is_err(),
            "Chrome directly connected after the SOCKS rejection"
        );
        spy.abort();
    }

    fn proxy_test_limits() -> OperationLimits {
        OperationLimits {
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            max_body_bytes: 1024,
            max_output_chars: 1024,
            max_redirects: 2,
        }
    }

    async fn run_proxy_request(request: &[u8]) -> (crate::Result<()>, Vec<u8>) {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let proxy = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let request = request.to_vec();
        let request = String::from_utf8(request)
            .unwrap()
            .replace("UPSTREAM", &upstream_address.to_string())
            .into_bytes();
        let allowlist = DestinationAllowlist::default();
        allowlist
            .authorize_url(
                &Url::parse(&format!("http://{upstream_address}/")).expect("fixture URL is valid"),
            )
            .unwrap();

        let proxy_task = tokio::spawn(async move {
            let (stream, _) = proxy.accept().await.unwrap();
            handle_policy_proxy(
                stream,
                NetworkPolicy::AllowPrivate,
                &proxy_test_limits(),
                &allowlist,
            )
            .await
        });
        let upstream_task = tokio::spawn(async move {
            let accepted =
                tokio::time::timeout(Duration::from_millis(300), upstream.accept()).await;
            let Ok(Ok((mut stream, _))) = accepted else {
                return Vec::new();
            };
            let mut bytes = Vec::new();
            let _ =
                tokio::time::timeout(Duration::from_millis(300), stream.read_to_end(&mut bytes))
                    .await;
            bytes
        });

        let mut client = TcpStream::connect(proxy_address).await.unwrap();
        client.write_all(&request).await.unwrap();
        client.shutdown().await.unwrap();
        let result = proxy_task.await.unwrap();
        let observed = upstream_task.await.unwrap();
        (result, observed)
    }

    #[tokio::test]
    async fn direct_proxy_never_forwards_a_pipelined_second_request() {
        let request = b"GET http://UPSTREAM/first HTTP/1.1\r\nHost: UPSTREAM\r\n\r\nGET http://forbidden.test/second HTTP/1.1\r\nHost: forbidden.test\r\n\r\n";
        let (result, observed) = run_proxy_request(request).await;

        assert!(result.is_err(), "pipelined request was accepted");
        assert!(observed.is_empty(), "upstream received pipelined bytes");
    }

    #[tokio::test]
    async fn direct_proxy_rejects_conflicting_framing_and_proxy_credentials() {
        for request in [
            b"POST http://UPSTREAM/ HTTP/1.1\r\nHost: UPSTREAM\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".as_slice(),
            b"GET http://UPSTREAM/ HTTP/1.1\r\nHost: UPSTREAM\r\nProxy-Authorization: Basic c2VjcmV0\r\n\r\n".as_slice(),
            b"GET http://UPSTREAM/ HTTP/1.1\r\nHost: wrong.example\r\n\r\n".as_slice(),
            b"GET http://UPSTREAM/ HTTP/1.1\r\nHost: UPSTREAM\r\nConnection: x-secret\r\nX-Secret: leak\r\n\r\n".as_slice(),
            b"GET http://UPSTREAM/ HTTP/2\r\nHost: UPSTREAM\r\n\r\n".as_slice(),
            b"GET https://UPSTREAM/ HTTP/1.1\r\nHost: UPSTREAM\r\n\r\n".as_slice(),
            b"CONNECT UPSTREAM/path HTTP/1.1\r\nHost: UPSTREAM\r\n\r\n".as_slice(),
        ] {
            let (result, observed) = run_proxy_request(request).await;
            assert!(result.is_err(), "restricted request was accepted");
            assert!(observed.is_empty(), "restricted request reached upstream");
        }
    }
}
