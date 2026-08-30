#[cfg(test)]
use crate::browser::{BrowserSetupPhase, BrowserSetupTestHook};
use crate::mime_policy::{reject_attachments, validate_content_type_declarations};
use crate::{Error, FetchHostRestriction, FetchRequest, NetworkPolicy, Result};
use async_tungstenite::tokio::{connect_async, ConnectStream};
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use url::{Host, Url};

const CDP_STREAM_CHUNK: usize = 16 * 1024;
const CDP_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Destination {
    host: String,
    port: u16,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DestinationAllowlist {
    destinations: Arc<Mutex<HashSet<Destination>>>,
}

impl DestinationAllowlist {
    pub(crate) fn authorize_url(&self, url: &Url) -> Result<()> {
        let port = url
            .port_or_known_default()
            .ok_or_else(|| Error::Policy("browser destination requires a port".into()))?;
        let host = url
            .host()
            .ok_or_else(|| Error::Policy("browser destination requires a host".into()))?;
        self.authorize_host(&host, port);
        Ok(())
    }

    pub(crate) fn authorize_host(&self, host: &Host<&str>, port: u16) {
        if let Ok(mut destinations) = self.destinations.lock() {
            destinations.insert(Destination {
                host: canonical_host(host),
                port,
            });
        }
    }

    pub(crate) fn allows(&self, host: &Host<String>, port: u16) -> bool {
        self.destinations
            .lock()
            .map(|destinations| {
                destinations.contains(&Destination {
                    host: canonical_owned_host(host),
                    port,
                })
            })
            .unwrap_or(false)
    }
}

fn canonical_host(host: &Host<&str>) -> String {
    match host {
        Host::Domain(domain) => domain.to_ascii_lowercase(),
        Host::Ipv4(address) => IpAddr::V4(*address).to_string(),
        Host::Ipv6(address) => IpAddr::V6(*address).to_string(),
    }
}

fn canonical_owned_host(host: &Host<String>) -> String {
    match host {
        Host::Domain(domain) => domain.to_ascii_lowercase(),
        Host::Ipv4(address) => IpAddr::V4(*address).to_string(),
        Host::Ipv6(address) => IpAddr::V6(*address).to_string(),
    }
}

#[derive(Clone, Debug)]
pub(crate) enum BrowserFailure {
    Policy(String),
    BodyLimit { limit: usize },
    Browser(String),
}

impl BrowserFailure {
    pub(crate) fn into_error(self) -> Error {
        match self {
            Self::Policy(message) => Error::Policy(message),
            Self::BodyLimit { limit } => Error::BodyLimit { limit },
            Self::Browser(message) => Error::Browser(message),
        }
    }
}

pub(crate) type SharedFailure = Arc<Mutex<Option<BrowserFailure>>>;
type CommandResult = std::result::Result<Value, String>;
type PendingCommands = Arc<Mutex<HashMap<u64, oneshot::Sender<CommandResult>>>>;

#[derive(Clone)]
struct RawCdp {
    next_id: Arc<AtomicU64>,
    outbound: mpsc::Sender<String>,
    pending: PendingCommands,
    timeout: Duration,
}

impl RawCdp {
    async fn execute(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> std::result::Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| "CDP pending-command state is poisoned".to_owned())?
            .insert(id, sender);
        let mut command = json!({"id": id, "method": method, "params": params});
        if let Some(session_id) = session_id {
            command["sessionId"] = Value::String(session_id.to_owned());
        }
        if self.outbound.send(command.to_string()).await.is_err() {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            return Err("CDP writer stopped".into());
        }
        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("CDP response channel closed".into()),
            Err(_) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                Err(format!("CDP command timed out: {method}"))
            }
        }
    }
}

struct RawEvent {
    method: String,
    params: Value,
    session_id: Option<String>,
}

#[derive(Clone)]
struct PolicyState {
    egress: BrowserPolicy,
    initial_url: Url,
    host_restriction: Option<FetchHostRestriction>,
    initial_headers: Vec<(String, String)>,
    initial_headers_applied: Arc<AtomicBool>,
    max_redirects: usize,
    redirects: Arc<AtomicUsize>,
    remaining_bytes: Arc<AtomicUsize>,
    body_limit: usize,
    final_content_type: Arc<Mutex<Option<String>>>,
    failure: SharedFailure,
    allowlist: DestinationAllowlist,
}

#[derive(Clone)]
pub(crate) enum BrowserPolicy {
    Direct(NetworkPolicy),
    Tor,
}

pub(crate) struct CdpController {
    command: RawCdp,
    event_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
    final_content_type: Arc<Mutex<Option<String>>>,
}

impl CdpController {
    pub(crate) async fn connect_websocket(
        websocket_url: &str,
        setup_timeout: Duration,
    ) -> Result<WebSocketStream<ConnectStream>> {
        tokio::time::timeout(setup_timeout, connect_async(websocket_url))
            .await
            .map_err(|_| Error::Timeout {
                operation: "browser policy controller setup",
            })?
            .map(|(websocket, _)| websocket)
            .map_err(|_| Error::Browser("failed to connect browser policy controller".into()))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn from_websocket(
        websocket: WebSocketStream<ConnectStream>,
        policy: BrowserPolicy,
        request: &FetchRequest,
        timeout: Duration,
        max_redirects: usize,
        max_body_bytes: usize,
        failure: SharedFailure,
        allowlist: DestinationAllowlist,
        task_counter: Arc<AtomicUsize>,
        #[cfg(test)] setup_hook: Option<Arc<BrowserSetupTestHook>>,
    ) -> Result<Self> {
        validate_caller_headers(request)?;
        let initial_headers = caller_headers(request)?;
        let (mut websocket_writer, mut websocket_reader) = websocket.split();
        let (outbound, mut outgoing) = mpsc::channel::<String>(128);
        let (events, mut incoming_events) = mpsc::channel::<RawEvent>(256);
        let pending: PendingCommands = Arc::new(Mutex::new(HashMap::new()));
        let writer_task = spawn_tracked(Arc::clone(&task_counter), async move {
            while let Some(message) = outgoing.recv().await {
                if websocket_writer
                    .send(Message::Text(message.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _ = websocket_writer.close(None).await;
        });
        #[cfg(test)]
        setup_checkpoint(&setup_hook, BrowserSetupPhase::Writer).await;
        let reader_pending = Arc::clone(&pending);
        let reader_task = spawn_tracked(Arc::clone(&task_counter), async move {
            while let Some(Ok(message)) = websocket_reader.next().await {
                let Ok(text) = message.into_text() else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(text.as_ref()) else {
                    continue;
                };
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    let sender = reader_pending
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.remove(&id));
                    if let Some(sender) = sender {
                        let result = match value.get("error") {
                            Some(error) => Err(error.to_string()),
                            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = sender.send(result);
                    }
                    continue;
                }
                let Some(method) = value.get("method").and_then(Value::as_str) else {
                    continue;
                };
                if events
                    .send(RawEvent {
                        method: method.to_owned(),
                        params: value.get("params").cloned().unwrap_or(Value::Null),
                        session_id: value
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        #[cfg(test)]
        setup_checkpoint(&setup_hook, BrowserSetupPhase::Reader).await;
        let command = RawCdp {
            next_id: Arc::new(AtomicU64::new(1)),
            outbound,
            pending,
            timeout,
        };
        let mut initial_url = request.url.clone();
        initial_url.set_fragment(None);
        let final_content_type = Arc::new(Mutex::new(None));
        let state = PolicyState {
            egress: policy,
            initial_url,
            host_restriction: request.host_restriction.clone(),
            initial_headers,
            initial_headers_applied: Arc::new(AtomicBool::new(false)),
            max_redirects,
            redirects: Arc::new(AtomicUsize::new(0)),
            remaining_bytes: Arc::new(AtomicUsize::new(max_body_bytes)),
            body_limit: max_body_bytes,
            final_content_type: Arc::clone(&final_content_type),
            failure,
            allowlist,
        };
        let event_command = command.clone();
        let event_task = spawn_tracked(task_counter, async move {
            while let Some(event) = incoming_events.recv().await {
                let result = match event.method.as_str() {
                    "Target.attachedToTarget" => {
                        configure_attached_target(&event_command, &event.params).await
                    }
                    "Fetch.requestPaused" => {
                        handle_paused_request(
                            &event_command,
                            event.session_id.as_deref(),
                            &state,
                            &event.params,
                        )
                        .await
                    }
                    _ => Ok(()),
                };
                if let Err(message) = result {
                    record_failure(&state.failure, BrowserFailure::Browser(message));
                }
            }
        });
        #[cfg(test)]
        setup_checkpoint(&setup_hook, BrowserSetupPhase::Event).await;
        let controller = Self {
            command,
            event_task,
            reader_task,
            writer_task,
            final_content_type,
        };

        if controller
            .command
            .execute(
                "Target.setAutoAttach",
                json!({
                    "autoAttach": true,
                    "waitForDebuggerOnStart": true,
                    "flatten": true
                }),
                None,
            )
            .await
            .is_err()
        {
            controller.stop_tasks(false).await;
            return Err(Error::Browser(
                "failed to enable target-wide browser policy".into(),
            ));
        }
        if controller
            .command
            .execute("Target.setDiscoverTargets", json!({"discover": true}), None)
            .await
            .is_err()
        {
            controller.stop_tasks(true).await;
            return Err(Error::Browser("failed to discover browser targets".into()));
        }

        Ok(controller)
    }

    pub(crate) async fn shutdown(self) {
        self.stop_tasks(true).await;
    }

    pub(crate) fn final_content_type(&self) -> Option<String> {
        self.final_content_type
            .lock()
            .ok()
            .and_then(|content_type| content_type.clone())
    }

    async fn stop_tasks(self, disable_auto_attach: bool) {
        if disable_auto_attach {
            let _ = tokio::time::timeout(
                CDP_SHUTDOWN_TIMEOUT,
                self.command.execute(
                    "Target.setAutoAttach",
                    json!({
                        "autoAttach": false,
                        "waitForDebuggerOnStart": false,
                        "flatten": true
                    }),
                    None,
                ),
            )
            .await;
        }
        self.event_task.abort();
        let _ = self.event_task.await;
        drop(self.command);
        self.writer_task.abort();
        let _ = self.writer_task.await;
        self.reader_task.abort();
        let _ = self.reader_task.await;
    }
}

struct TaskCounterGuard(Arc<AtomicUsize>);

impl Drop for TaskCounterGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn spawn_tracked<F>(counter: Arc<AtomicUsize>, future: F) -> JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    counter.fetch_add(1, Ordering::SeqCst);
    let guard = TaskCounterGuard(counter);
    tokio::spawn(async move {
        let _guard = guard;
        future.await;
    })
}

#[cfg(test)]
async fn setup_checkpoint(hook: &Option<Arc<BrowserSetupTestHook>>, phase: BrowserSetupPhase) {
    let Some(hook) = hook.as_ref().filter(|hook| hook.phase == phase) else {
        return;
    };
    struct CheckpointGuard<'a> {
        hook: &'a BrowserSetupTestHook,
        released: bool,
    }
    impl Drop for CheckpointGuard<'_> {
        fn drop(&mut self) {
            if !self.released {
                self.hook
                    .dropped_before_release
                    .store(true, Ordering::SeqCst);
            }
        }
    }
    let mut guard = CheckpointGuard {
        hook,
        released: false,
    };
    hook.reached.notify_waiters();
    if let Ok(permit) = hook.release.acquire().await {
        permit.forget();
        guard.released = true;
    }
}

async fn configure_attached_target(
    command: &RawCdp,
    params: &Value,
) -> std::result::Result<(), String> {
    let session = required_string(params, "sessionId")?;
    let target_type = params
        .pointer("/targetInfo/type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    command
        .execute(
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true
            }),
            Some(session),
        )
        .await?;
    command
        .execute(
            "Fetch.enable",
            json!({
                "patterns": [
                    {"urlPattern": "*", "requestStage": "Request"},
                    {"urlPattern": "*", "requestStage": "Response"}
                ],
                "handleAuthRequests": false
            }),
            Some(session),
        )
        .await
        .map_err(|error| format!("failed to enforce Fetch policy in {target_type}: {error}"))?;
    command
        .execute("Runtime.runIfWaitingForDebugger", json!({}), Some(session))
        .await?;
    Ok(())
}

async fn handle_paused_request(
    command: &RawCdp,
    session: Option<&str>,
    state: &PolicyState,
    params: &Value,
) -> std::result::Result<(), String> {
    let request_id = required_string(params, "requestId")?;
    if params.get("responseStatusCode").is_some() || params.get("responseErrorReason").is_some() {
        return handle_response(command, session, state, params, request_id).await;
    }

    let url = params
        .pointer("/request/url")
        .and_then(Value::as_str)
        .ok_or_else(|| "paused browser request has no URL".to_owned())?;
    let url =
        Url::parse(url).map_err(|_| "paused browser request has an invalid URL".to_owned())?;
    let resource_type = params
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or("Other");
    let redirected = params.get("redirectedRequestId").is_some();
    let allowed = validate_and_authorize_request(state, &url, resource_type, redirected).is_ok();
    if !allowed {
        record_failure(
            &state.failure,
            BrowserFailure::Policy("browser target request violated policy".into()),
        );
        let _ = command
            .execute(
                "Fetch.failRequest",
                json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
                session,
            )
            .await;
        return Ok(());
    }
    let mut continue_params = json!({"requestId": request_id});
    if request_is_initial_document(params, &url, &state.initial_url)
        && state
            .initial_headers_applied
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let browser_headers = params
            .pointer("/request/headers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        continue_params["headers"] =
            Value::Array(merge_headers(browser_headers, &state.initial_headers));
    }
    command
        .execute("Fetch.continueRequest", continue_params, session)
        .await?;
    Ok(())
}

fn validate_request(
    state: &PolicyState,
    url: &Url,
    resource_type: &str,
    redirected: bool,
) -> Result<()> {
    if redirected && state.redirects.fetch_add(1, Ordering::Relaxed) >= state.max_redirects {
        return Err(Error::Policy("browser redirect limit exceeded".into()));
    }
    match state.egress {
        BrowserPolicy::Direct(policy) => crate::policy::validate_url(url, policy)?,
        BrowserPolicy::Tor => crate::policy::validate_url(url, NetworkPolicy::PublicInternet)?,
    }
    if let Some(restriction) = &state.host_restriction {
        restriction.validate(url)?;
    }
    if !matches!(
        resource_type,
        "Document" | "Stylesheet" | "Script" | "XHR" | "Fetch" | "Preflight"
    ) {
        return Err(Error::Policy(format!(
            "browser resource type is forbidden: {resource_type}"
        )));
    }
    Ok(())
}

fn validate_and_authorize_request(
    state: &PolicyState,
    url: &Url,
    resource_type: &str,
    redirected: bool,
) -> Result<()> {
    validate_request(state, url, resource_type, redirected)?;
    state.allowlist.authorize_url(url)
}

async fn handle_response(
    command: &RawCdp,
    session: Option<&str>,
    state: &PolicyState,
    params: &Value,
    request_id: &str,
) -> std::result::Result<(), String> {
    if params.get("responseErrorReason").is_some() {
        command
            .execute(
                "Fetch.continueResponse",
                json!({"requestId": request_id}),
                session,
            )
            .await?;
        return Ok(());
    }
    let status = params
        .get("responseStatusCode")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let resource_type = params
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or("Other");
    let headers = params
        .get("responseHeaders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        command
            .execute(
                "Fetch.fulfillRequest",
                json!({
                    "requestId": request_id,
                    "responseCode": status,
                    "responseHeaders": normalized_response_headers(headers, 0, false),
                    "body": ""
                }),
                session,
            )
            .await?;
        return Ok(());
    }
    if resource_type == "Document" {
        match validate_document_response(&headers) {
            Ok(content_type) => {
                let mut final_content_type = state
                    .final_content_type
                    .lock()
                    .map_err(|_| "browser content-type state is poisoned".to_owned())?;
                if final_content_type.is_none() {
                    final_content_type.replace(content_type);
                }
            }
            Err(failure) => {
                record_failure(&state.failure, failure);
                let _ = command
                    .execute(
                        "Fetch.failRequest",
                        json!({"requestId": request_id, "errorReason": "BlockedByResponse"}),
                        session,
                    )
                    .await;
                return Ok(());
            }
        }
    }
    if let Some(length) = unique_content_length(&headers).map_err(|failure| {
        record_failure(&state.failure, failure);
        "invalid response Content-Length".to_owned()
    })? {
        if length > state.remaining_bytes.load(Ordering::Relaxed) {
            record_failure(
                &state.failure,
                BrowserFailure::BodyLimit {
                    limit: state.body_limit,
                },
            );
            let _ = command
                .execute(
                    "Fetch.failRequest",
                    json!({"requestId": request_id, "errorReason": "BlockedByResponse"}),
                    session,
                )
                .await;
            return Ok(());
        }
    }

    let stream = command
        .execute(
            "Fetch.takeResponseBodyAsStream",
            json!({"requestId": request_id}),
            session,
        )
        .await?
        .get("stream")
        .and_then(Value::as_str)
        .ok_or_else(|| "CDP did not return a response stream".to_owned())?
        .to_owned();
    let mut body = Vec::new();
    loop {
        let remaining = state.remaining_bytes.load(Ordering::Relaxed);
        let read_size = remaining.saturating_add(1).clamp(1, CDP_STREAM_CHUNK);
        let chunk = command
            .execute(
                "IO.read",
                json!({"handle": stream, "size": read_size}),
                session,
            )
            .await?;
        let data = chunk.get("data").and_then(Value::as_str).unwrap_or("");
        let decoded = if chunk
            .get("base64Encoded")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| "CDP returned invalid base64 response data".to_owned())?
        } else {
            data.as_bytes().to_vec()
        };
        if decoded.len() > remaining {
            record_failure(
                &state.failure,
                BrowserFailure::BodyLimit {
                    limit: state.body_limit,
                },
            );
            let _ = command
                .execute("IO.close", json!({"handle": stream}), session)
                .await;
            let _ = command
                .execute(
                    "Fetch.failRequest",
                    json!({"requestId": request_id, "errorReason": "BlockedByResponse"}),
                    session,
                )
                .await;
            return Ok(());
        }
        if !decoded.is_empty() {
            state
                .remaining_bytes
                .fetch_sub(decoded.len(), Ordering::Relaxed);
            body.extend_from_slice(&decoded);
        }
        if chunk.get("eof").and_then(Value::as_bool).unwrap_or(false) {
            break;
        }
    }
    let _ = command
        .execute("IO.close", json!({"handle": stream}), session)
        .await;
    command
        .execute(
            "Fetch.fulfillRequest",
            json!({
                "requestId": request_id,
                "responseCode": status,
                "responseHeaders": normalized_response_headers(
                    headers,
                    body.len(),
                    resource_type == "Document"
                ),
                "body": base64::engine::general_purpose::STANDARD.encode(body)
            }),
            session,
        )
        .await?;
    Ok(())
}

fn validate_document_response(headers: &[Value]) -> std::result::Result<String, BrowserFailure> {
    let content_types = header_values(headers, "content-type");
    let validated = validate_content_type_declarations(&content_types)
        .map_err(|error| BrowserFailure::Policy(error.to_string()))?;
    if !validated.identity.media_type.is_html_document() {
        return Err(BrowserFailure::Policy(
            "browser document MIME is not HTML".into(),
        ));
    }
    reject_attachments(&header_values(headers, "content-disposition"))
        .map_err(|error| BrowserFailure::Policy(error.to_string()))?;
    Ok(validated.declaration)
}

fn unique_content_length(headers: &[Value]) -> std::result::Result<Option<usize>, BrowserFailure> {
    let values = header_values(headers, "content-length");
    if values.is_empty() {
        return Ok(None);
    }
    let first = values[0].trim();
    if values.iter().any(|value| value.trim() != first) {
        return Err(BrowserFailure::Policy(
            "browser response has conflicting Content-Length fields".into(),
        ));
    }
    first
        .parse()
        .map(Some)
        .map_err(|_| BrowserFailure::Policy("browser response has invalid Content-Length".into()))
}

fn header_values<'a>(headers: &'a [Value], wanted: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter_map(|header| {
            let name = header.get("name")?.as_str()?;
            name.eq_ignore_ascii_case(wanted)
                .then(|| header.get("value")?.as_str())
                .flatten()
        })
        .collect()
}

fn normalized_response_headers(
    mut headers: Vec<Value>,
    body_length: usize,
    isolated_document: bool,
) -> Vec<Value> {
    headers.retain(|header| {
        !header
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("content-length")
                    || name.eq_ignore_ascii_case("transfer-encoding")
                    || name.eq_ignore_ascii_case("connection")
            })
    });
    headers.push(json!({"name": "Content-Length", "value": body_length.to_string()}));
    if isolated_document {
        headers.push(json!({
            "name": "Content-Security-Policy",
            "value": "worker-src 'none'; child-src 'none'; frame-src 'none'; object-src 'none'; media-src 'none'; img-src 'none'; font-src 'none'; connect-src http: https:; sandbox allow-scripts allow-same-origin"
        }));
    }
    headers
}

fn request_is_initial_document(params: &Value, url: &Url, initial_url: &Url) -> bool {
    params.get("resourceType").and_then(Value::as_str) == Some("Document") && url == initial_url
}

fn merge_headers(
    browser_headers: Map<String, Value>,
    caller_headers: &[(String, String)],
) -> Vec<Value> {
    let mut merged = browser_headers
        .into_iter()
        .filter_map(|(name, value)| value.as_str().map(|value| (name, value.to_owned())))
        .collect::<Vec<_>>();
    for (caller_name, caller_value) in caller_headers {
        merged.retain(|(name, _)| !name.eq_ignore_ascii_case(caller_name));
        merged.push((caller_name.clone(), caller_value.clone()));
    }
    merged
        .into_iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect()
}

pub(crate) fn validate_caller_headers(request: &FetchRequest) -> Result<()> {
    let mut names = HashSet::new();
    for (name, _) in &request.headers {
        let lower = name.as_str().to_ascii_lowercase();
        if !names.insert(lower.clone()) {
            return Err(Error::InvalidInput(
                "duplicate browser request headers are not supported".into(),
            ));
        }
        if is_restricted_request_header(&lower) {
            return Err(Error::InvalidInput(format!(
                "restricted browser request header: {lower}"
            )));
        }
    }
    Ok(())
}

fn caller_headers(request: &FetchRequest) -> Result<Vec<(String, String)>> {
    request
        .headers
        .iter()
        .map(|(name, value)| {
            Ok((
                name.as_str().to_owned(),
                value
                    .to_str()
                    .map_err(|_| Error::InvalidInput("invalid browser header value".into()))?
                    .to_owned(),
            ))
        })
        .collect()
}

fn is_restricted_request_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "proxy-connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
    ) || name.starts_with("proxy-")
}

fn required_string<'a>(value: &'a Value, key: &str) -> std::result::Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("CDP event is missing {key}"))
}

fn record_failure(failure: &SharedFailure, value: BrowserFailure) {
    if let Ok(mut failure) = failure.lock() {
        if failure.is_none() {
            *failure = Some(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        handle_paused_request, spawn_tracked, validate_and_authorize_request,
        validate_caller_headers, BrowserPolicy, DestinationAllowlist, PolicyState, RawCdp,
    };
    use crate::{Error, FetchHostRestriction, FetchRequest};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use url::{Host, Url};

    fn exact_host_policy_state() -> PolicyState {
        let initial =
            Url::parse("http://aebagbafaydqqcikbmga2dqpcaireeyuculbogazdinryhi6d4qcmeqd.onion/")
                .unwrap();
        PolicyState {
            egress: BrowserPolicy::Tor,
            initial_url: initial.clone(),
            host_restriction: Some(
                FetchHostRestriction::http_or_https_exact_host(&initial).unwrap(),
            ),
            initial_headers: Vec::new(),
            initial_headers_applied: Arc::new(AtomicBool::new(false)),
            max_redirects: 8,
            redirects: Arc::new(AtomicUsize::new(0)),
            remaining_bytes: Arc::new(AtomicUsize::new(1_024)),
            body_limit: 1_024,
            final_content_type: Arc::new(Mutex::new(None)),
            failure: Arc::new(Mutex::new(None)),
            allowlist: DestinationAllowlist::default(),
        }
    }

    #[tokio::test]
    async fn aborting_an_unpolled_tracked_task_releases_its_accounting() {
        let counter = Arc::new(AtomicUsize::new(0));
        let task = spawn_tracked(Arc::clone(&counter), std::future::pending());
        task.abort();
        let _ = task.await;

        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn caller_headers_reject_routing_framing_proxy_and_duplicate_fields() {
        for name in [
            "host",
            "connection",
            "content-length",
            "transfer-encoding",
            "proxy-authorization",
            "proxy-connection",
            "keep-alive",
            "te",
            "trailer",
            "upgrade",
        ] {
            let mut request = FetchRequest::browser("https://example.com/").unwrap();
            request.headers.insert(name, "value".parse().unwrap());
            assert!(
                validate_caller_headers(&request).is_err(),
                "accepted {name}"
            );
        }

        let mut duplicate = FetchRequest::browser("https://example.com/").unwrap();
        duplicate
            .headers
            .append("x-rscraper-test", "one".parse().unwrap());
        duplicate
            .headers
            .append("x-rscraper-test", "two".parse().unwrap());
        assert!(validate_caller_headers(&duplicate).is_err());

        let mut safe = FetchRequest::browser("https://example.com/").unwrap();
        safe.headers
            .insert("x-rscraper-test", "survives".parse().unwrap());
        safe.headers
            .insert("user-agent", "fixture-agent".parse().unwrap());
        assert!(validate_caller_headers(&safe).is_ok());
    }

    #[test]
    fn exact_host_interception_authorizes_same_host_resources_and_never_cross_host() {
        let state = exact_host_policy_state();
        let initial = state.initial_url.clone();
        let allowlist = state.allowlist.clone();

        for resource_type in [
            "Document",
            "Stylesheet",
            "Script",
            "XHR",
            "Fetch",
            "Preflight",
        ] {
            let same_host = initial.join(&format!("/{resource_type}")).unwrap();
            validate_and_authorize_request(&state, &same_host, resource_type, false).unwrap();
        }
        let same_host_redirect = initial.join("/redirected").unwrap();
        validate_and_authorize_request(&state, &same_host_redirect, "Document", true).unwrap();

        let cross_host = Url::parse(
            "http://aibqibiga4eascqlbqgq4dyqcejbgfavcylrqgi2dmob2hq7eaqs4eqd.onion/script.js",
        )
        .unwrap();
        assert!(validate_and_authorize_request(&state, &cross_host, "Script", false).is_err());
        assert!(validate_and_authorize_request(&state, &cross_host, "Document", true).is_err());
        assert!(!allowlist.allows(
            &Host::Domain(cross_host.host_str().unwrap().to_owned()),
            cross_host.port_or_known_default().unwrap()
        ));

        let mut unrestricted = state.clone();
        unrestricted.host_restriction = None;
        validate_and_authorize_request(&unrestricted, &cross_host, "Script", false).unwrap();
    }

    #[tokio::test]
    async fn paused_request_policy_failure_redacts_every_intercepted_url_component() {
        const CROSS_HOST: &str = "aibqibiga4eascqlbqgq4dyqcejbgfavcylrqgi2dmob2hq7eaqs4eqd.onion";
        let intercepted = [
            format!(
                "http://{CROSS_HOST}/secret-path?secret-query=secret-value#secret-fragment"
            ),
            format!(
                "http://secret-user:secret-password@{CROSS_HOST}/credential-path?credential-query=credential-value"
            ),
        ];

        for candidate in intercepted {
            let state = exact_host_policy_state();
            let (outbound, _commands) = mpsc::channel(4);
            let command = RawCdp {
                next_id: Arc::new(AtomicU64::new(1)),
                outbound,
                pending: Arc::new(Mutex::new(HashMap::new())),
                timeout: Duration::ZERO,
            };
            handle_paused_request(
                &command,
                None,
                &state,
                &json!({
                    "requestId": "blocked-request",
                    "request": {"url": candidate},
                    "resourceType": "Script"
                }),
            )
            .await
            .unwrap();

            let failure = state.failure.lock().unwrap().take().unwrap().into_error();
            assert!(matches!(&failure, Error::Policy(_)));
            let public = format!("{failure}\n{failure:?}");
            for sentinel in [
                CROSS_HOST,
                "secret-path",
                "secret-query",
                "secret-value",
                "secret-fragment",
                "secret-user",
                "secret-password",
                "credential-path",
                "credential-query",
                "credential-value",
            ] {
                assert!(
                    !public.contains(sentinel),
                    "public browser policy error exposed {sentinel}: {public}"
                );
            }
        }
    }
}
