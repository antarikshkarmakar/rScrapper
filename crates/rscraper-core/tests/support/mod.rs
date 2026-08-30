#![allow(dead_code)]

use rscraper_core::ResolverSource;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use url::Url;

#[derive(Clone, Default)]
pub struct StaticResolver {
    answers: Arc<HashMap<String, Vec<SocketAddr>>>,
}

impl StaticResolver {
    pub fn single(host: &str, addresses: Vec<IpAddr>) -> Self {
        Self::new([(host, addresses)])
    }

    pub fn new<const N: usize>(answers: [(&str, Vec<IpAddr>); N]) -> Self {
        let answers = answers
            .into_iter()
            .map(|(host, addresses)| {
                (
                    host.to_ascii_lowercase(),
                    addresses
                        .into_iter()
                        .map(|address| SocketAddr::new(address, 0))
                        .collect(),
                )
            })
            .collect();
        Self {
            answers: Arc::new(answers),
        }
    }
}

impl ResolverSource for StaticResolver {
    fn resolve(
        &self,
        host: String,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>> {
        let result = self
            .answers
            .get(&host.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "fixture host not found"));
        Box::pin(async move { result })
    }
}

#[derive(Clone)]
pub enum ResponseBody {
    Fixed(Vec<u8>),
    Delayed { delay: Duration, bytes: Vec<u8> },
    Chunks(Vec<(Duration, Vec<u8>)>),
}

#[derive(Clone)]
pub struct TestResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
}

impl TestResponse {
    pub fn html(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
            body: ResponseBody::Fixed(body.into()),
        }
    }

    pub fn redirect(location: impl Into<String>) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".into(), location.into())],
            body: ResponseBody::Fixed(Vec::new()),
        }
    }
}

pub struct TestServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl TestServer {
    pub async fn spawn(
        routes: impl IntoIterator<Item = (impl Into<String>, TestResponse)>,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let routes: Arc<HashMap<String, TestResponse>> = Arc::new(
            routes
                .into_iter()
                .map(|(path, response)| (path.into(), response))
                .collect(),
        );
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                tokio::spawn(async move {
                    serve_connection(stream, routes, None).await;
                });
            }
        });
        Self { address, task }
    }

    pub async fn spawn_recording(
        routes: impl IntoIterator<Item = (impl Into<String>, TestResponse)>,
    ) -> (Self, Arc<Mutex<Vec<CapturedRequest>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let routes: Arc<HashMap<String, TestResponse>> = Arc::new(
            routes
                .into_iter()
                .map(|(path, response)| (path.into(), response))
                .collect(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    serve_connection(stream, routes, Some(recorded)).await;
                });
            }
        });
        (Self { address, task }, requests)
    }

    pub fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{}", self.address, path)).unwrap()
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct TestTlsServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl TestTlsServer {
    pub async fn spawn(response: TestResponse) -> Self {
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["tls.test".into()]).unwrap();
        let certificate = cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = Arc::new(response);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let response = Arc::clone(&response);
                tokio::spawn(async move {
                    if let Ok(stream) = acceptor.accept(stream).await {
                        serve_connection(
                            stream,
                            Arc::new(HashMap::from([("/".into(), (*response).clone())])),
                            None,
                        )
                        .await;
                    }
                });
            }
        });
        Self { address, task }
    }

    pub fn url(&self) -> Url {
        Url::parse(&format!("https://tls.test:{}/", self.address.port())).unwrap()
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for TestTlsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Debug)]
pub struct CapturedRequest {
    pub target: String,
    pub headers: HashMap<String, String>,
}

async fn serve_connection<S>(
    mut stream: S,
    routes: Arc<HashMap<String, TestResponse>>,
    recorded: Option<Arc<Mutex<Vec<CapturedRequest>>>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }

    let request_text = std::str::from_utf8(&request).unwrap_or_default();
    let path = request_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    if let Some(recorded) = recorded {
        let headers = request_text
            .lines()
            .skip(1)
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        recorded.lock().unwrap().push(CapturedRequest {
            target: path.to_owned(),
            headers,
        });
    }
    let response = routes
        .get(path)
        .cloned()
        .unwrap_or_else(|| TestResponse::html(404, "not found"));
    write_response(&mut stream, response).await;
}

async fn write_response<S>(stream: &mut S, response: TestResponse)
where
    S: AsyncWrite + Unpin,
{
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        _ => "Test Response",
    };
    let mut headers = response.headers;
    let is_chunked = matches!(response.body, ResponseBody::Chunks(_));
    if is_chunked {
        headers.push(("Transfer-Encoding".into(), "chunked".into()));
    } else if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        let length = match &response.body {
            ResponseBody::Fixed(bytes) | ResponseBody::Delayed { bytes, .. } => bytes.len(),
            ResponseBody::Chunks(_) => unreachable!(),
        };
        headers.push(("Content-Length".into(), length.to_string()));
    }
    headers.push(("Connection".into(), "close".into()));

    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason);
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }

    match response.body {
        ResponseBody::Fixed(bytes) => {
            let _ = stream.write_all(&bytes).await;
        }
        ResponseBody::Delayed { delay, bytes } => {
            tokio::time::sleep(delay).await;
            let _ = stream.write_all(&bytes).await;
        }
        ResponseBody::Chunks(chunks) => {
            for (delay, bytes) in chunks {
                tokio::time::sleep(delay).await;
                if stream
                    .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                    .await
                    .is_err()
                    || stream.write_all(&bytes).await.is_err()
                    || stream.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
        }
    }
}
