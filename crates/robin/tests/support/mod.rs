#![allow(dead_code)]

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};
use url::Url;

pub struct RecordedRequest {
    pub head: String,
    pub body: Vec<u8>,
}

pub async fn one_shot_server(response: Vec<u8>) -> (Url, oneshot::Receiver<RecordedRequest>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, request_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "request ended before its headers");
            received.extend_from_slice(&chunk[..read]);
            if let Some(index) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let head = String::from_utf8(received[..header_end].to_vec()).unwrap();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or_default();
        while received.len() - header_end < content_length {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "request ended before its body");
            received.extend_from_slice(&chunk[..read]);
        }
        request_tx
            .send(RecordedRequest {
                head,
                body: received[header_end..header_end + content_length].to_vec(),
            })
            .ok();
        stream.write_all(&response).await.unwrap();
    });
    (
        Url::parse(&format!("http://{address}/")).unwrap(),
        request_rx,
    )
}

pub async fn hanging_server() -> Url {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer).await;
        std::future::pending::<()>().await;
    });
    Url::parse(&format!("http://{address}/")).unwrap()
}

pub fn json_response(status: u16, body: &str, extra_headers: &[(&str, &str)]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    response.push_str(body);
    response.into_bytes()
}

#[derive(Clone, Default)]
pub struct CallLog(pub Arc<Mutex<Vec<String>>>);

impl CallLog {
    pub async fn push(&self, value: impl Into<String>) {
        self.0.lock().await.push(value.into());
    }

    pub async fn snapshot(&self) -> Vec<String> {
        self.0.lock().await.clone()
    }
}
