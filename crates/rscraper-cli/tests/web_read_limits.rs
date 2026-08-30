use std::path::PathBuf;

use rscraper_cli::{context::AppContext, web::read_with_max_chars};
use rscraper_core::{Error, FetchClient, NetworkPolicy};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn public_bounded_read_seam_reports_its_exact_character_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2_048];
        let _ = stream.read(&mut request).await.unwrap();
        let body = "<main>a🦀bZ</main>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let context = AppContext {
        fetch: FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .build()
            .unwrap(),
        browser: None,
        config_dir: PathBuf::new(),
    };

    let error = read_with_max_chars(&context, &format!("http://{address}/"), 3)
        .await
        .unwrap_err();

    assert!(matches!(error, Error::BodyLimit { limit: 3 }));
    fixture.await.unwrap();
}
