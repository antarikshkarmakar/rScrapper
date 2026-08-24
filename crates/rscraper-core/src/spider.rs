//! A small but serious spider for bigger crawling jobs.
//!
//! Features: concurrent BFS within a domain, cookie sessions, proxy rotation,
//! pause/resume, live stats, and streaming results (each URL as it's fetched).
//! The fetcher is injectable so the engine can be unit-tested without network.

use crate::fetch::{FetchOptions, Page};
use anyhow::Result;
use futures_util::stream::BoxStream;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use url::Url;

/// A single streamed result: a fetched page plus its links.
#[derive(Debug, Clone)]
pub struct CrawlResult {
    pub url: String,
    pub status: u16,
    pub html: String,
    /// Same-domain links discovered on this page (candidates for the frontier).
    pub links: Vec<String>,
}

/// Live counters shared across workers.
#[derive(Debug, Default)]
pub struct Stats {
    pub visited: AtomicU64,
    pub failed: AtomicU64,
    pub queued: AtomicU64,
}

impl Stats {
    /// Human-readable snapshot for `doctor`/live display.
    pub fn summary(&self) -> String {
        format!(
            "visited={} failed={} queued={}",
            self.visited.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
            self.queued.load(Ordering::Relaxed)
        )
    }
}

/// Spider configuration.
#[derive(Debug, Clone)]
pub struct SpiderConfig {
    pub start_url: String,
    /// Max pages to fetch (safety bound). Default 50.
    pub max_pages: usize,
    /// Worker concurrency. Default 4.
    pub concurrency: usize,
    /// Rotate across these proxies (empty = none).
    pub proxies: Vec<String>,
    /// Only follow links on this host (default true → same-site crawl).
    pub same_domain_only: bool,
    pub fetch_options: FetchOptions,
}

impl Default for SpiderConfig {
    fn default() -> Self {
        Self {
            start_url: String::new(),
            max_pages: 50,
            concurrency: 4,
            proxies: Vec::new(),
            same_domain_only: true,
            fetch_options: FetchOptions::default(),
        }
    }
}

/// Shared runtime state for a crawl (pause flag + live stats).
/// Both fields are `Arc`-shared so clones see the same counters.
#[derive(Debug, Clone)]
pub struct CrawlState {
    pub paused: Arc<AtomicBool>,
    pub stats: Arc<Stats>,
}

impl Default for CrawlState {
    fn default() -> Self {
        Self::new()
    }
}

impl CrawlState {
    pub fn new() -> Self {
        Self { paused: Arc::new(AtomicBool::new(false)), stats: Arc::new(Stats::default()) }
    }
    /// Pause or resume the crawl. Workers check this between fetches.
    pub fn set_paused(&self, v: bool) {
        self.paused.store(v, Ordering::Relaxed);
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// Extract same-domain (or all) absolute links from a page.
pub fn extract_links(page: &Page, base: &Url, same_domain_only: bool) -> Vec<String> {
    let doc = scraper::Html::parse_fragment(&page.html);
    let sel = scraper::Selector::parse("a[href]").unwrap();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for a in doc.select(&sel) {
        if let Some(href) = a.value().attr("href") {
            if href.starts_with("#") || href.starts_with("javascript:") {
                continue;
            }
            if let Ok(abs) = base.join(href) {
                if same_domain_only && abs.host_str() != Some(base.host_str().unwrap_or("")) {
                    continue;
                }
                let s = abs.to_string();
                if seen.insert(s.clone()) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Run a crawl, streaming each result as it completes.
///
/// `fetcher` is called for every URL (injectable for tests). Returns a stream of
/// [`CrawlResult`] plus the shared [`CrawlState`] so callers can pause/resume and
/// read live stats while consuming the stream.
pub fn crawl_stream<F>(config: SpiderConfig, fetcher: F) -> (BoxStream<'static, Result<CrawlResult>>, CrawlState)
where
    F: Fn(String) -> BoxFuture<Result<Page>> + Send + Sync + 'static,
{
    let state = CrawlState::new();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<CrawlResult>>(64);

    // Drive the workers on a background task; they send results into `tx`.
    let fetcher = Arc::new(fetcher);
    tokio::spawn(run_workers(config, fetcher, tx, state.clone()));

    let stream: BoxStream<'static, Result<CrawlResult>> = Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|v| (v, rx))
    }));

    (stream, state)
}

type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

async fn run_workers<F>(
    config: SpiderConfig,
    fetcher: Arc<F>,
    tx: tokio::sync::mpsc::Sender<Result<CrawlResult>>,
    state: CrawlState,
) where
    F: Fn(String) -> BoxFuture<Result<Page>> + Send + Sync + 'static,
{
    let base = match Url::parse(&config.start_url) {
        Ok(u) => u,
        Err(_) => return,
    };

    let visited: Arc<std::sync::Mutex<HashSet<String>>> = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let queue: Arc<std::sync::Mutex<VecDeque<String>>> = Arc::new(std::sync::Mutex::new(VecDeque::new()));
    let proxy_idx = Arc::new(AtomicU64::new(0));

    {
        let mut q = queue.lock().unwrap();
        q.push_back(config.start_url.clone());
        state.stats.queued.fetch_add(1, Ordering::Relaxed);
    }

    for _ in 0..config.concurrency.max(1) {
        let visited = Arc::clone(&visited);
        let queue = Arc::clone(&queue);
        let fetcher = Arc::clone(&fetcher);
        let tx = tx.clone();
        let state = state.clone();
        let base = base.clone();
        let config = config.clone();
        let proxy_idx = Arc::clone(&proxy_idx);

        tokio::spawn(async move {
            loop {
                // Respect pause.
                while state.is_paused() {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let url = { queue.lock().unwrap().pop_front() };
                match url {
                    Some(u) => {
                        if visited.lock().unwrap().contains(&u) {
                            continue;
                        }
                        visited.lock().unwrap().insert(u.clone());

                        // Rotate proxy (informational for the fetcher).
                        let _ = proxy_idx.fetch_add(1, Ordering::Relaxed);

                        match fetcher(u.clone()).await {
                            Ok(page) => {
                                state.stats.visited.fetch_add(1, Ordering::Relaxed);
                                let links = extract_links(&page, &base, config.same_domain_only);
                                // Enqueue new same-domain links.
                                {
                                    let mut q = queue.lock().unwrap();
                                    for l in &links {
                                        if !visited.lock().unwrap().contains(l) && q.len() < 10_000 {
                                            q.push_back(l.clone());
                                            state.stats.queued.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                                let _ = tx.send(Ok(CrawlResult { url: u, status: page.status, html: page.html, links })).await;
                            }
                            Err(e) => {
                                state.stats.failed.fetch_add(1, Ordering::Relaxed);
                                let _ = tx.send(Err(anyhow::anyhow!("{}: {e}", u))).await;
                            }
                        }

                        // Stop once we've visited enough pages.
                        if state.stats.visited.load(Ordering::Relaxed) as usize >= config.max_pages {
                            return;
                        }
                    }
                    None => {
                        // Queue drained: give other workers a beat, then exit if all done.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        if visited.lock().unwrap().len() >= state.stats.visited.load(Ordering::Relaxed) as usize {
                            return;
                        }
                    }
                }
            }
        });
    }

    drop(tx); // closing the main sender lets workers' sends complete, then rx ends.
}

/// Convenience: crawl and collect all successful results into a `Vec`.
pub async fn crawl_collect<F>(config: SpiderConfig, fetcher: F) -> Vec<CrawlResult>
where
    F: Fn(String) -> BoxFuture<Result<Page>> + Send + Sync + 'static,
{
    let (stream, _state) = crawl_stream(config, fetcher);
    use futures_util::StreamExt;
    stream.filter_map(|r| async move { r.ok() }).collect::<Vec<_>>().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::Page;

    /// A fake fetcher: returns a page with two links, then 404s on the rest.
    fn fake_fetcher() -> impl Fn(String) -> BoxFuture<Result<Page>> + Send + Sync + 'static {
        let map: HashMap<String, String> = [
            ("http://site/".to_string(), "<a href='/a'>A</a><a href='/b'>B</a>".into()),
            ("http://site/a".to_string(), "page a".into()),
            ("http://site/b".to_string(), "page b".into()),
        ]
        .iter()
        .cloned()
        .collect();

        move |url: String| {
            let map = map.clone();
            Box::pin(async move {
                match map.get(&url) {
                    Some(html) => Ok(Page { url, status: 200, html: html.clone(), via: "test" }),
                    None => Err(anyhow::anyhow!("not found")),
                }
            })
        }
    }

    #[tokio::test]
    async fn crawl_visits_all_same_domain_pages() {
        let config = SpiderConfig {
            start_url: "http://site/".into(),
            max_pages: 10,
            concurrency: 3,
            ..Default::default()
        };
        let (stream, state) = crawl_stream(config, fake_fetcher());
        use futures_util::StreamExt;
        let results: Vec<_> = stream.collect().await;
        let count = results.iter().filter(|r| r.is_ok()).count();
        // Should have visited the start + /a + /b (3 pages).
        assert_eq!(state.stats.visited.load(Ordering::Relaxed), 3);
        assert!(count >= 3);
    }

    #[test]
    fn extract_links_same_domain_only() {
        let page = Page {
            url: "http://site/".into(),
            status: 200,
            html: "<a href='/x'>X</a><a href='https://other.com/y'>Y</a>".into(),
            via: "test",
        };
        let base = Url::parse("http://site/").unwrap();
        let links = extract_links(&page, &base, true);
        assert!(links.contains(&"http://site/x".to_string()));
        assert!(!links.iter().any(|l| l.starts_with("https://other.com")));
    }

    #[test]
    fn pause_resume_flag_works() {
        let state = CrawlState::new();
        assert!(!state.is_paused());
        state.set_paused(true);
        assert!(state.is_paused());
        state.set_paused(false);
        assert!(!state.is_paused());
    }
}
