use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use url::Url;

#[derive(Clone, Debug)]
pub struct FixtureResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FixtureResponse {
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn text(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn redirect(location: impl Into<String>) -> Self {
        Self {
            status: 302,
            content_type: "text/plain; charset=utf-8",
            headers: vec![("Location".into(), location.into())],
            body: Vec::new(),
        }
    }

    pub fn unsupported() -> Self {
        Self {
            status: 200,
            content_type: "application/octet-stream",
            headers: Vec::new(),
            body: b"unsupported".to_vec(),
        }
    }

    pub fn headerless(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "",
            headers: Vec::new(),
            body: body.into(),
        }
    }
}

#[derive(Clone)]
pub enum FixtureAction {
    Respond(FixtureResponse),
    Delay(Duration, FixtureResponse),
    Wait(Arc<Semaphore>, FixtureResponse),
}

#[derive(Clone, Debug)]
pub struct ObservedRequest {
    pub target: String,
    pub started_at: Instant,
    pub headers: HashMap<String, String>,
}

type Handler = Arc<dyn Fn(&str) -> FixtureAction + Send + Sync>;

pub struct ControlledServer {
    address: SocketAddr,
    state: Arc<ServerState>,
    task: JoinHandle<()>,
}

struct ServerState {
    requests: Mutex<Vec<ObservedRequest>>,
    active: AtomicUsize,
    maximum_active: AtomicUsize,
    changed: Notify,
}

impl ControlledServer {
    pub async fn spawn(handler: impl Fn(&str) -> FixtureAction + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(ServerState {
            requests: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            maximum_active: AtomicUsize::new(0),
            changed: Notify::new(),
        });
        let server_state = Arc::clone(&state);
        let handler: Handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let state = Arc::clone(&server_state);
                        let handler = Arc::clone(&handler);
                        connections.spawn(async move {
                            serve_connection(stream, state, handler).await;
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            address,
            state,
            task,
        }
    }

    pub fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{}", self.address, path)).unwrap()
    }

    pub fn proxy_url(&self) -> Url {
        self.url("/")
    }

    pub fn request_targets(&self) -> Vec<String> {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.target.clone())
            .collect()
    }

    pub fn requests(&self) -> Vec<ObservedRequest> {
        self.state.requests.lock().unwrap().clone()
    }

    pub fn active(&self) -> usize {
        self.state.active.load(Ordering::SeqCst)
    }

    pub fn maximum_active(&self) -> usize {
        self.state.maximum_active.load(Ordering::SeqCst)
    }

    pub async fn wait_for_requests(&self, expected: usize) {
        wait_until(&self.state, || {
            self.state.requests.lock().unwrap().len() >= expected
        })
        .await;
    }

    pub async fn wait_for_idle(&self) {
        wait_until(&self.state, || self.active() == 0).await;
    }
}

impl Drop for ControlledServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_until(state: &ServerState, predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let changed = state.changed.notified();
            if predicate() {
                return;
            }
            changed.await;
        }
    })
    .await
    .expect("fixture server observation timed out");
}

struct ActiveRequest {
    state: Arc<ServerState>,
}

impl ActiveRequest {
    fn begin(state: Arc<ServerState>, target: String, headers: HashMap<String, String>) -> Self {
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.maximum_active.fetch_max(active, Ordering::SeqCst);
        state.requests.lock().unwrap().push(ObservedRequest {
            target,
            started_at: Instant::now(),
            headers,
        });
        state.changed.notify_waiters();
        Self { state }
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        self.state.changed.notify_waiters();
    }
}

async fn serve_connection(mut stream: TcpStream, state: Arc<ServerState>, handler: Handler) {
    let Some((target, headers)) = read_request(&mut stream).await else {
        return;
    };
    let _active = ActiveRequest::begin(state, target.clone(), headers);
    match handler(&target) {
        FixtureAction::Respond(response) => write_response(&mut stream, response).await,
        FixtureAction::Delay(delay, response) => {
            tokio::time::sleep(delay).await;
            write_response(&mut stream, response).await;
        }
        FixtureAction::Wait(gate, response) => {
            let mut disconnect_probe = [0u8; 1];
            tokio::select! {
                permit = gate.acquire_owned() => {
                    if let Ok(permit) = permit {
                        permit.forget();
                        write_response(&mut stream, response).await;
                    }
                }
                _ = stream.read(&mut disconnect_probe) => {}
            }
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, HashMap<String, String>)> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() > 16 * 1024 {
            return None;
        }
    }
    let request = std::str::from_utf8(&request).ok()?;
    let mut lines = request.lines();
    let target = lines.next()?.split_whitespace().nth(1)?.to_owned();
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    Some((target, headers))
}

fn write_response<'a>(
    stream: &'a mut TcpStream,
    response: FixtureResponse,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let reason = match response.status {
            200 => "OK",
            302 => "Found",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Fixture",
        };
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            reason,
            response.body.len()
        );
        if !response.content_type.is_empty() {
            head.push_str(&format!("Content-Type: {}\r\n", response.content_type));
        }
        for (name, value) in response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        if stream.write_all(head.as_bytes()).await.is_ok() {
            let _ = stream.write_all(&response.body).await;
        }
    })
}
