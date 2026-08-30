use rscraper_cli::{
    context::AppContext,
    rss::{fetch_feed_items_with_context, parse_feed_bytes},
};
use rscraper_core::{Error, FetchClient, NetworkPolicy, OperationLimits};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

#[test]
fn rss_namespaces_cdata_relative_guid_and_unusable_entries_are_normalized() {
    let feed_url = Url::parse("https://example.com/news/rss.xml").unwrap();
    let items =
        parse_feed_bytes(include_bytes!("fixtures/rss-namespaced.xml"), &feed_url, 20).unwrap();

    assert_eq!(items.len(), 2);
    assert_eq!(items[0].title, "RSS & News");
    assert_eq!(items[0].link, "https://example.com/posts/alpha");
    assert_eq!(items[0].description, "First **line**  \nSecond & final.");
    assert_eq!(items[0].date, "2024-08-27T14:15:16+00:00");
    assert_eq!(items[1].title, "GUID Fallback");
    assert_eq!(items[1].link, "https://example.com/guid-post");
    assert_eq!(items[1].description, "Plain & simple");
}

#[test]
fn atom_cdata_prefers_alternate_links_and_rfc3339_dates() {
    let feed_url = Url::parse("https://example.com/feeds/atom.xml").unwrap();
    let items = parse_feed_bytes(include_bytes!("fixtures/atom-cdata.xml"), &feed_url, 20).unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Atom \"Title\" & More");
    assert_eq!(items[0].link, "https://example.com/articles/atom-entry");
    assert_eq!(items[0].description, "Atom *summary* & details");
    assert_eq!(items[0].date, "2024-09-02T03:04:05+00:00");
}

#[test]
fn json_feed_items_use_structural_content_and_relative_urls() {
    let feed_url = Url::parse("https://example.com/json/feed.json").unwrap();
    let items = parse_feed_bytes(include_bytes!("fixtures/json-feed.json"), &feed_url, 20).unwrap();

    assert_eq!(items.len(), 2);
    assert_eq!(items[0].title, "JSON Entry");
    assert_eq!(items[0].link, "https://example.com/json/one");
    assert_eq!(items[0].description, "JSON & **HTML**");
    assert_eq!(items[0].date, "2024-10-03T04:05:06+00:00");
    assert_eq!(items[1].title, "Text Entry");
    assert_eq!(items[1].link, "https://example.com/json/two");
    assert_eq!(items[1].description, "Plain text & entities");
}

#[test]
fn partial_feed_items_are_kept_without_fabricating_generated_id_links() {
    let feed_url = Url::parse("https://example.com/news/rss.xml").unwrap();
    let xml = br#"<rss version="2.0"><channel>
      <item>
        <title>Title and link only</title>
        <link>/title-link</link>
      </item>
      <item>
        <title>Title and description only</title>
        <description>Body only</description>
      </item>
      <item>
        <link>/link-description</link>
        <description>Linked body</description>
      </item>
      <item>
        <title>Explicit GUID fallback</title>
        <guid isPermaLink="true">/guid-link</guid>
      </item>
      <item>
        <title>Non-permalink GUID is not a link</title>
        <guid isPermaLink="false">not-a-url</guid>
        <description>Stable body</description>
      </item>
      <item>
        <title> </title>
        <description> </description>
      </item>
    </channel></rss>"#;

    let items = parse_feed_bytes(xml, &feed_url, 20).unwrap();

    assert_eq!(items.len(), 5);
    assert_eq!(items[0].title, "Title and link only");
    assert_eq!(items[0].link, "https://example.com/title-link");
    assert_eq!(items[0].description, "");
    assert_eq!(items[1].title, "Title and description only");
    assert_eq!(items[1].link, "");
    assert_eq!(items[1].description, "Body only");
    assert_eq!(items[2].title, "");
    assert_eq!(items[2].link, "https://example.com/link-description");
    assert_eq!(items[2].description, "Linked body");
    assert_eq!(items[3].title, "Explicit GUID fallback");
    assert_eq!(items[3].link, "https://example.com/guid-link");
    assert_eq!(items[3].description, "");
    assert_eq!(items[4].title, "Non-permalink GUID is not a link");
    assert_eq!(items[4].link, "");
    assert_eq!(items[4].description, "Stable body");
}

#[test]
fn explicit_atom_ids_can_fall_back_to_links() {
    let feed_url = Url::parse("https://example.com/feeds/atom.xml").unwrap();
    let xml = br#"<feed xmlns="http://www.w3.org/2005/Atom">
      <entry>
        <title>Atom ID fallback</title>
        <id>/atom-id-link</id>
      </entry>
    </feed>"#;

    let items = parse_feed_bytes(xml, &feed_url, 20).unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Atom ID fallback");
    assert_eq!(items[0].link, "https://example.com/atom-id-link");
    assert_eq!(items[0].description, "");
}

#[test]
fn feed_limit_is_hard_capped_at_one_hundred() {
    let feed_url = Url::parse("https://example.com/feed.json").unwrap();
    let mut items = Vec::new();
    for index in 0..150 {
        items.push(serde_json::json!({
            "id": format!("item-{index}"),
            "url": format!("/item-{index}"),
            "title": format!("Item {index}"),
            "content_text": format!("Body {index}")
        }));
    }
    let feed = serde_json::json!({
        "version": "https://jsonfeed.org/version/1.1",
        "title": "Many",
        "items": items
    });
    let bytes = serde_json::to_vec(&feed).unwrap();

    assert_eq!(parse_feed_bytes(&bytes, &feed_url, 7).unwrap().len(), 7);
    assert_eq!(parse_feed_bytes(&bytes, &feed_url, 150).unwrap().len(), 100);
}

#[test]
fn dtd_external_entities_are_rejected_without_expansion() {
    let feed_url = Url::parse("https://example.com/rss.xml").unwrap();
    let bytes = br#"<!DOCTYPE rss [
        <!ENTITY local SYSTEM "file:///etc/passwd">
    ]>
    <rss><channel>
      <item>
        <title>Unsafe</title>
        <link>https://example.com/unsafe</link>
        <description>&local;</description>
      </item>
    </channel></rss>"#;

    let error = parse_feed_bytes(bytes, &feed_url, 20).unwrap_err();
    assert!(matches!(error, Error::Parse { kind: "feed", .. }));

    let mut bom_prefixed = b"\xEF\xBB\xBF".to_vec();
    bom_prefixed.extend_from_slice(bytes);
    let error = parse_feed_bytes(&bom_prefixed, &feed_url, 20).unwrap_err();
    assert!(matches!(error, Error::Parse { kind: "feed", .. }));
}

#[tokio::test]
async fn live_feed_fetch_passes_original_bytes_to_feed_parser() {
    let iso_8859_1_feed =
        b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><rss version=\"2.0\"><channel><item><title>Caf\xe9</title><link>https://example.com/cafe</link></item></channel></rss>";
    let server = OneShotServer::spawn("application/rss+xml", iso_8859_1_feed.as_slice()).await;
    let context = AppContext {
        fetch: FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .limits(OperationLimits {
                connect_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_secs(5),
                max_body_bytes: 4096,
                max_output_chars: 4096,
                max_redirects: 3,
            })
            .build()
            .unwrap(),
        browser: None,
        config_dir: PathBuf::new(),
    };

    let items = fetch_feed_items_with_context(&context, server.url().as_str(), 20)
        .await
        .unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Café");
}

struct OneShotServer {
    address: std::net::SocketAddr,
}

impl OneShotServer {
    async fn spawn(content_type: &'static str, body: &[u8]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_vec();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
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
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(&body).await;
        });
        Self { address }
    }

    fn url(&self) -> Url {
        Url::parse(&format!("http://{}/latin1.xml", self.address)).unwrap()
    }
}
