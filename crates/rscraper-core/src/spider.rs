//! Bounded, policy-aware crawling with one central frontier.

use crate::document::Page;
use crate::robots::RobotsPolicy;
use crate::urlnorm::{
    is_destructive_url, normalize_url, resolve_and_normalize, same_origin, within_origin_scope,
    Origin,
};
use crate::{Error, FetchClient, FetchRequest, FetchStep, Result, RobotsFetchStep};
use futures_util::stream::{BoxStream, FuturesUnordered};
use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderValue, USER_AGENT};
use scraper::{Html, Selector};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

const MAX_PAGES: usize = 100;
const MAX_CONCURRENCY: usize = 16;
const MAX_PROXIES: usize = 8;
const MAX_PENDING_DISCOVERIES: usize = MAX_LINKS_PER_PAGE;
const MAX_SEEN_URLS: usize = 1 + MAX_PAGES * MAX_LINKS_PER_PAGE;

/// Product token sent on every crawler-owned robots and page request.
pub const CRAWLER_USER_AGENT: &str = "rscraper";

/// Maximum unique, policy-eligible links retained in one [`CrawlResult`].
pub const MAX_LINKS_PER_PAGE: usize = 1_024;

/// Fully explicit crawl scheduler configuration.
#[derive(Clone)]
pub struct CrawlConfig {
    /// First normalized page reservation.
    pub start_url: Url,
    /// Maximum page reservations, from 1 through 100.
    pub max_pages: usize,
    /// Maximum in-flight work, from 1 through 16.
    pub concurrency: usize,
    /// Keep discovered page URLs on the start origin.
    pub same_origin_only: bool,
    /// Permit subdomains when origin scoping is enabled.
    pub include_subdomains: bool,
    /// Fetch and enforce robots.txt once per origin.
    pub respect_robots: bool,
    /// Minimum spacing between page starts for one origin.
    pub minimum_delay: Duration,
    /// Optional rotating validated proxies.
    pub proxies: Vec<Url>,
}

impl fmt::Debug for CrawlConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CrawlConfig")
            .field("start_url", &Redacted)
            .field("max_pages", &self.max_pages)
            .field("concurrency", &self.concurrency)
            .field("same_origin_only", &self.same_origin_only)
            .field("include_subdomains", &self.include_subdomains)
            .field("respect_robots", &self.respect_robots)
            .field("minimum_delay", &self.minimum_delay)
            .field("proxy_count", &self.proxies.len())
            .finish()
    }
}

/// One successfully fetched crawl page and its bounded eligible links.
#[derive(Clone)]
pub struct CrawlResult {
    /// Final normalized page URL.
    pub url: Url,
    /// HTTP status retained for caller policy.
    pub status: u16,
    /// Decoded bounded HTML.
    pub html: String,
    /// Unique normalized links retained from this page.
    pub links: Vec<Url>,
}

impl fmt::Debug for CrawlResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CrawlResult")
            .field("url", &Redacted)
            .field("status", &self.status)
            .field("html_len", &self.html.len())
            .field("link_count", &self.links.len())
            .finish()
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Debug, Clone)]
pub struct CrawlControl {
    pub cancellation: CancellationToken,
    pause: watch::Sender<bool>,
}

impl CrawlControl {
    /// Pause or resume new scheduler starts without cancelling in-flight work.
    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.pause.send(paused).map_err(|_| Error::Cancelled)
    }

    /// Cancel the crawl. Dropping the output stream has the same effect.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrawlStatsSnapshot {
    /// Requests that started.
    pub attempted: u64,
    /// Page requests that completed successfully.
    pub succeeded: u64,
    /// Started work that produced an error.
    pub failed: u64,
    /// Reservations admitted to the frontier.
    pub queued: u64,
    /// Discoveries rejected by bounds, scope, robots, or deduplication.
    pub skipped: u64,
}

#[derive(Debug, Default)]
pub struct CrawlStats {
    snapshot: Mutex<CrawlStatsSnapshot>,
}

impl CrawlStats {
    /// Copy the current counters.
    pub fn snapshot(&self) -> CrawlStatsSnapshot {
        *self.lock_snapshot()
    }

    /// Render counters without URLs or other remote data.
    pub fn summary(&self) -> String {
        let snapshot = self.snapshot();
        format!(
            "attempted={} succeeded={} failed={} queued={} skipped={}",
            snapshot.attempted,
            snapshot.succeeded,
            snapshot.failed,
            snapshot.queued,
            snapshot.skipped
        )
    }

    fn update(&self, update: impl FnOnce(&mut CrawlStatsSnapshot)) {
        update(&mut self.lock_snapshot());
    }

    fn lock_snapshot(&self) -> std::sync::MutexGuard<'_, CrawlStatsSnapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone)]
pub struct Crawler {
    fetch: FetchClient,
}

impl Crawler {
    /// Build a crawler around one reusable, policy-enforcing transport.
    pub fn new(fetch: FetchClient) -> Self {
        Self { fetch }
    }

    /// Start a bounded crawl.
    ///
    /// Configuration and every known proxy/destination incompatibility are
    /// rejected before scheduler state or background tasks are created.
    /// A page reservation receives its rotating proxy before robots checks;
    /// the first reservation inspected for an origin supplies the proxy for
    /// that origin's once-per-crawl robots cache fill.
    ///
    /// Initial page attempts denied by robots and robots HTTP failures are
    /// emitted as typed errors. Later discovered denials are counted as
    /// skipped. Dropping the returned stream cancels scheduler work.
    pub fn stream(
        &self,
        mut config: CrawlConfig,
    ) -> Result<(
        BoxStream<'static, Result<CrawlResult>>,
        CrawlControl,
        Arc<CrawlStats>,
    )> {
        validate_config(&config)?;
        self.fetch.request_deadline()?;
        config.start_url = normalize_url(&config.start_url)?;
        let mut preflight = crawler_request(&config.start_url)?;
        self.fetch.preflight_request(&preflight)?;
        for proxy in &config.proxies {
            preflight.proxy = Some(proxy.clone());
            self.fetch.preflight_request(&preflight)?;
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::InvalidInput("crawler requires a Tokio runtime".into()))?;

        let cancellation = CancellationToken::new();
        let (pause, pause_rx) = watch::channel(false);
        let control = CrawlControl {
            cancellation: cancellation.clone(),
            pause,
        };
        let stats = Arc::new(CrawlStats::default());
        stats.update(|snapshot| snapshot.queued = 1);
        let (output, receiver) = mpsc::channel(config.concurrency.saturating_mul(2).max(1));
        let scheduler = Scheduler::new(
            self.fetch.clone(),
            config,
            Arc::clone(&stats),
            cancellation.clone(),
            pause_rx,
            output,
        );
        runtime.spawn(scheduler.run());

        let stream: BoxStream<'static, Result<CrawlResult>> = Box::pin(CrawlReceiver {
            receiver,
            cancellation,
        });
        Ok((stream, control, stats))
    }
}

struct CrawlReceiver {
    receiver: mpsc::Receiver<Result<CrawlResult>>,
    cancellation: CancellationToken,
}

impl Stream for CrawlReceiver {
    type Item = Result<CrawlResult>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

impl Drop for CrawlReceiver {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

type FetchTask = Pin<Box<dyn Future<Output = TaskCompletion> + Send>>;

enum TaskCompletion {
    Robots {
        origin: Origin,
        redirects: usize,
        deadline: Instant,
        result: Result<RobotsFetchStep>,
    },
    Page {
        attempt_url: Url,
        redirects: usize,
        deadline: Instant,
        result: Result<FetchStep>,
    },
    Cancelled,
}

struct PageReservation {
    url: Url,
    proxy: Option<Url>,
}

enum FrontierItem {
    Page(PageReservation),
    Redirect {
        attempt_url: Url,
        redirects: usize,
        deadline: Instant,
        request: FetchRequest,
    },
    RobotsRedirect {
        origin: Origin,
        redirects: usize,
        deadline: Instant,
        request: FetchRequest,
    },
}

impl FrontierItem {
    fn url(&self) -> &Url {
        match self {
            Self::Page(reservation) => &reservation.url,
            Self::Redirect { request, .. } => &request.url,
            Self::RobotsRedirect { request, .. } => &request.url,
        }
    }

    fn proxy(&self) -> Option<&Url> {
        match self {
            Self::Page(reservation) => reservation.proxy.as_ref(),
            Self::Redirect { request, .. } => request.proxy.as_ref(),
            Self::RobotsRedirect { request, .. } => request.proxy.as_ref(),
        }
    }

    fn reserves_page(&self) -> bool {
        matches!(self, Self::Page(_))
    }

    fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Page(_) => None,
            Self::Redirect { deadline, .. } | Self::RobotsRedirect { deadline, .. } => {
                Some(*deadline)
            }
        }
    }
}

enum RobotsState {
    Unchecked,
    Fetching,
    Ready(RobotsPolicy),
    Failed,
}

struct OriginState {
    robots: RobotsState,
    last_page_start: Option<Instant>,
}

impl OriginState {
    fn new(respect_robots: bool) -> Self {
        Self {
            robots: if respect_robots {
                RobotsState::Unchecked
            } else {
                RobotsState::Ready(RobotsPolicy::default())
            },
            last_page_start: None,
        }
    }
}

struct Scheduler {
    fetch: FetchClient,
    config: CrawlConfig,
    stats: Arc<CrawlStats>,
    cancellation: CancellationToken,
    pause: watch::Receiver<bool>,
    output: mpsc::Sender<Result<CrawlResult>>,
    frontier: VecDeque<FrontierItem>,
    pending_discoveries: VecDeque<Url>,
    seen: HashSet<Url>,
    owned: HashSet<Url>,
    completed: HashSet<Url>,
    origins: HashMap<Origin, OriginState>,
    in_flight: FuturesUnordered<FetchTask>,
    scheduled_pages: usize,
    page_policy_owner: Option<Url>,
}

impl Scheduler {
    fn new(
        fetch: FetchClient,
        config: CrawlConfig,
        stats: Arc<CrawlStats>,
        cancellation: CancellationToken,
        pause: watch::Receiver<bool>,
        output: mpsc::Sender<Result<CrawlResult>>,
    ) -> Self {
        let start_url = config.start_url.clone();
        let owned_start = start_url.clone();
        Self {
            fetch,
            config,
            stats,
            cancellation,
            pause,
            output,
            frontier: VecDeque::from([FrontierItem::Page(PageReservation {
                url: start_url.clone(),
                proxy: None,
            })]),
            pending_discoveries: VecDeque::new(),
            seen: HashSet::from([start_url]),
            owned: HashSet::from([owned_start]),
            completed: HashSet::new(),
            origins: HashMap::new(),
            in_flight: FuturesUnordered::new(),
            scheduled_pages: 0,
            page_policy_owner: None,
        }
    }

    async fn run(mut self) {
        loop {
            if self.output.is_closed() {
                self.cancellation.cancel();
            }
            if self.cancellation.is_cancelled() {
                self.reap_cancelled().await;
                return;
            }

            if *self.pause.borrow() {
                if let Some(completion) = self.wait_while_paused().await {
                    if !self.handle_completion(completion).await {
                        self.reap_cancelled().await;
                        return;
                    }
                }
                continue;
            }

            let earliest_ready = self.fill_capacity().await;
            if self.scheduled_pages == self.config.max_pages {
                self.discard_pending_discoveries();
            }
            if self.cancellation.is_cancelled() {
                self.reap_cancelled().await;
                return;
            }
            if self.frontier.is_empty()
                && self.pending_discoveries.is_empty()
                && self.in_flight.is_empty()
            {
                return;
            }

            let completion = match (self.in_flight.is_empty(), earliest_ready) {
                (false, Some(deadline)) => {
                    tokio::select! {
                        biased;
                        _ = self.cancellation.cancelled() => None,
                        completion = self.in_flight.next() => completion,
                        _ = tokio::time::sleep_until(deadline) => continue,
                    }
                }
                (false, None) => {
                    tokio::select! {
                        biased;
                        _ = self.cancellation.cancelled() => None,
                        completion = self.in_flight.next() => completion,
                    }
                }
                (true, Some(deadline)) => {
                    tokio::select! {
                        biased;
                        _ = self.cancellation.cancelled() => None,
                        _ = tokio::time::sleep_until(deadline) => continue,
                    }
                }
                (true, None) => return,
            };

            let Some(completion) = completion else {
                self.cancellation.cancel();
                self.reap_cancelled().await;
                return;
            };
            if !self.handle_completion(completion).await {
                self.reap_cancelled().await;
                return;
            }
        }
    }

    async fn wait_while_paused(&mut self) -> Option<TaskCompletion> {
        if self.in_flight.is_empty() {
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => None,
                resumed = self.pause.wait_for(|paused| !*paused) => {
                    if resumed.is_err() {
                        self.cancellation.cancel();
                    }
                    None
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => None,
                completion = self.in_flight.next() => completion,
                resumed = self.pause.wait_for(|paused| !*paused) => {
                    if resumed.is_err() {
                        self.cancellation.cancel();
                    }
                    None
                }
            }
        }
    }

    async fn fill_capacity(&mut self) -> Option<Instant> {
        self.promote_pending_discoveries();
        if !self.expire_frontier_deadlines().await {
            return None;
        }
        let mut earliest_ready = self
            .frontier
            .iter()
            .filter_map(FrontierItem::deadline)
            .min();
        loop {
            if self.in_flight.len() >= self.config.concurrency
                || self.frontier.is_empty()
                || *self.pause.borrow()
            {
                break;
            }
            let candidates = self.frontier.len();
            let mut made_progress = false;
            for _ in 0..candidates {
                if self.in_flight.len() >= self.config.concurrency {
                    break;
                }
                let Some(item) = self.frontier.pop_front() else {
                    break;
                };
                let mut item = match item {
                    FrontierItem::RobotsRedirect {
                        origin,
                        redirects,
                        deadline,
                        request,
                    } => {
                        self.schedule_robots_hop(origin, redirects, deadline, request);
                        made_progress = true;
                        continue;
                    }
                    item => item,
                };
                if matches!(&item, FrontierItem::Page(reservation) if self.completed.contains(&reservation.url))
                {
                    self.release_page_policy_owner(&item);
                    self.increment_skipped();
                    self.promote_pending_discoveries();
                    made_progress = true;
                    continue;
                }
                let Ok(origin) = Origin::from_url(item.url()) else {
                    self.release_page_policy_owner(&item);
                    if !self.reject_item(item, false).await {
                        return earliest_ready;
                    }
                    self.promote_pending_discoveries();
                    made_progress = true;
                    continue;
                };
                self.origins
                    .entry(origin.clone())
                    .or_insert_with(|| OriginState::new(self.config.respect_robots));

                let robots_action = {
                    let state = self.origins.get(&origin).expect("origin inserted");
                    match &state.robots {
                        RobotsState::Unchecked => RobotsAction::Fetch,
                        RobotsState::Fetching => RobotsAction::Wait,
                        RobotsState::Failed => RobotsAction::Deny { failed: true },
                        RobotsState::Ready(policy) if !policy.allows(item.url()) => {
                            RobotsAction::Deny { failed: false }
                        }
                        RobotsState::Ready(policy) => RobotsAction::Ready(policy.crawl_delay()),
                    }
                };
                if !self.config.proxies.is_empty() && matches!(item, FrontierItem::Page(_)) {
                    let page_url = item.url().clone();
                    let owns_policy_slot = self.page_policy_owner.as_ref() == Some(&page_url);
                    if self.page_policy_owner.is_some() && !owns_policy_slot {
                        self.frontier.push_back(item);
                        continue;
                    }
                    if self.page_policy_owner.is_none()
                        && matches!(robots_action, RobotsAction::Fetch)
                    {
                        let proxy = proxy_for_index(&self.config.proxies, self.scheduled_pages);
                        let FrontierItem::Page(reservation) = &mut item else {
                            unreachable!("page admission checked a non-page item")
                        };
                        reservation.proxy = proxy;
                        self.page_policy_owner = Some(page_url);
                    } else if self.page_policy_owner.is_none()
                        && matches!(robots_action, RobotsAction::Wait)
                    {
                        // This origin's cache transaction belongs to an
                        // already-attempted redirect. Do not reserve an
                        // attempted index until this page can actually start.
                        self.frontier.push_back(item);
                        continue;
                    }
                }
                match robots_action {
                    RobotsAction::Fetch => {
                        self.origins
                            .get_mut(&origin)
                            .expect("origin inserted")
                            .robots = RobotsState::Fetching;
                        let proxy = item.proxy().cloned();
                        self.frontier.push_back(item);
                        self.schedule_robots(origin, proxy);
                        made_progress = true;
                    }
                    RobotsAction::Wait => self.frontier.push_back(item),
                    RobotsAction::Deny { failed } => {
                        self.release_page_policy_owner(&item);
                        if !self.reject_item(item, failed).await {
                            return earliest_ready;
                        }
                        self.promote_pending_discoveries();
                        made_progress = true;
                    }
                    RobotsAction::Ready(robots_delay) => {
                        let delay = self
                            .config
                            .minimum_delay
                            .max(robots_delay.unwrap_or_default());
                        let ready_at = self
                            .origins
                            .get(&origin)
                            .and_then(|state| state.last_page_start)
                            .and_then(|last| last.checked_add(delay));
                        if ready_at.is_some_and(|ready| ready > Instant::now()) {
                            earliest_ready = min_instant(earliest_ready, ready_at);
                            self.frontier.push_back(item);
                        } else {
                            self.prepare_page_for_schedule(&mut item);
                            self.release_page_policy_owner(&item);
                            self.origins
                                .get_mut(&origin)
                                .expect("origin inserted")
                                .last_page_start = Some(Instant::now());
                            self.schedule_frontier_item(item);
                            self.promote_pending_discoveries();
                            made_progress = true;
                        }
                    }
                }
            }
            if !made_progress {
                break;
            }
        }
        earliest_ready
    }

    async fn expire_frontier_deadlines(&mut self) -> bool {
        let now = Instant::now();
        let mut pending = std::mem::take(&mut self.frontier);
        let origins_with_live_waiters = pending
            .iter()
            .filter(|item| match item {
                FrontierItem::Page(_) => true,
                FrontierItem::Redirect { deadline, .. } => *deadline > now,
                FrontierItem::RobotsRedirect { .. } => false,
            })
            .filter_map(|item| Origin::from_url(item.url()).ok())
            .collect::<HashSet<_>>();
        let mut emitted_origin_errors = HashSet::new();
        while let Some(item) = pending.pop_front() {
            if item.deadline().is_none_or(|deadline| deadline > now) {
                self.frontier.push_back(item);
                continue;
            }
            match item {
                FrontierItem::Redirect { attempt_url, .. } => {
                    if !self
                        .fail_attempt(
                            attempt_url,
                            Error::Timeout {
                                operation: "request",
                            },
                        )
                        .await
                    {
                        return false;
                    }
                }
                FrontierItem::RobotsRedirect { origin, .. } => {
                    self.origins
                        .get_mut(&origin)
                        .expect("robots origin exists")
                        .robots = RobotsState::Failed;
                    if origins_with_live_waiters.contains(&origin)
                        && emitted_origin_errors.insert(origin)
                        && !self
                            .emit(Err(Error::Timeout {
                                operation: "request",
                            }))
                            .await
                    {
                        return false;
                    }
                }
                FrontierItem::Page(_) => unreachable!("page reservations have no deadline"),
            }
        }
        true
    }

    /// A page that starts a new robots transaction temporarily owns the next
    /// attempted-index proxy. Policy denial/failure releases that index for
    /// the next page; an admitted page carries the same proxy onto the wire.
    fn schedule_robots(&mut self, origin: Origin, proxy: Option<Url>) {
        let mut request = crawler_request(&origin.robots_url())
            .expect("validated origin makes a valid robots request");
        request.proxy = proxy;
        let deadline = match self.fetch.request_deadline() {
            Ok(deadline) => deadline,
            Err(error) => {
                let deadline = Instant::now();
                self.in_flight.push(Box::pin(async move {
                    TaskCompletion::Robots {
                        origin,
                        redirects: 0,
                        deadline,
                        result: Err(error),
                    }
                }));
                return;
            }
        };
        self.schedule_robots_hop(origin, 0, deadline, request);
    }

    fn schedule_robots_hop(
        &mut self,
        origin: Origin,
        redirects: usize,
        deadline: Instant,
        request: FetchRequest,
    ) {
        let fetch = self.fetch.clone();
        let cancellation = self.cancellation.clone();
        self.in_flight.push(Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => TaskCompletion::Cancelled,
                result = fetch.fetch_robots_one_hop_until(request, deadline) => TaskCompletion::Robots {
                    origin,
                    redirects,
                    deadline,
                    result,
                },
            }
        }));
    }

    fn schedule_frontier_item(&mut self, item: FrontierItem) {
        match item {
            FrontierItem::Page(reservation) => {
                self.scheduled_pages += 1;
                self.stats.update(|snapshot| snapshot.attempted += 1);
                let mut request = crawler_request(&reservation.url)
                    .expect("validated frontier URL makes a valid request");
                request.proxy = reservation.proxy;
                match self.fetch.request_deadline() {
                    Ok(deadline) => self.schedule_page_hop(reservation.url, 0, deadline, request),
                    Err(error) => {
                        let attempt_url = reservation.url;
                        let deadline = Instant::now();
                        self.in_flight.push(Box::pin(async move {
                            TaskCompletion::Page {
                                attempt_url,
                                redirects: 0,
                                deadline,
                                result: Err(error),
                            }
                        }));
                    }
                }
            }
            FrontierItem::Redirect {
                attempt_url,
                redirects,
                deadline,
                request,
            } => self.schedule_page_hop(attempt_url, redirects, deadline, request),
            FrontierItem::RobotsRedirect {
                origin,
                redirects,
                deadline,
                request,
            } => self.schedule_robots_hop(origin, redirects, deadline, request),
        }
    }

    fn schedule_page_hop(
        &mut self,
        attempt_url: Url,
        redirects: usize,
        deadline: Instant,
        request: FetchRequest,
    ) {
        let fetch = self.fetch.clone();
        let cancellation = self.cancellation.clone();
        self.in_flight.push(Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => TaskCompletion::Cancelled,
                result = fetch.fetch_request_one_hop_until(request, deadline) => TaskCompletion::Page {
                    attempt_url,
                    redirects,
                    deadline,
                    result,
                },
            }
        }));
    }

    async fn handle_completion(&mut self, completion: TaskCompletion) -> bool {
        match completion {
            TaskCompletion::Cancelled => true,
            TaskCompletion::Robots {
                origin,
                redirects,
                deadline,
                result,
            } => {
                self.handle_robots_completion(origin, redirects, deadline, result)
                    .await
            }
            TaskCompletion::Page {
                attempt_url,
                redirects,
                deadline,
                result,
            } => {
                self.handle_page_completion(attempt_url, redirects, deadline, result)
                    .await
            }
        }
    }

    async fn handle_robots_completion(
        &mut self,
        origin: Origin,
        redirects: usize,
        deadline: Instant,
        result: Result<RobotsFetchStep>,
    ) -> bool {
        let policy = match result {
            Ok(RobotsFetchStep::Missing { .. }) => Ok(Some(RobotsPolicy::default())),
            Ok(RobotsFetchStep::Text { text, .. }) => {
                Ok(Some(RobotsPolicy::parse(&text, CRAWLER_USER_AGENT)))
            }
            Ok(RobotsFetchStep::Status { url, status }) => Err(Error::HttpStatus { status, url }),
            Ok(RobotsFetchStep::Redirect(redirect)) => {
                if redirects >= self.fetch.limits().max_redirects {
                    Err(Error::Policy("robots.txt redirect limit exceeded".into()))
                } else if !same_origin(&origin.robots_url(), &redirect.next_request.url) {
                    Err(Error::Policy(
                        "cross-origin robots.txt redirect is not permitted".into(),
                    ))
                } else {
                    self.frontier.push_front(FrontierItem::RobotsRedirect {
                        origin: origin.clone(),
                        redirects: redirects + 1,
                        deadline,
                        request: redirect.next_request,
                    });
                    Ok(None)
                }
            }
            Err(error) => Err(error),
        };

        match policy {
            Ok(Some(policy)) => {
                self.origins
                    .get_mut(&origin)
                    .expect("robots origin exists")
                    .robots = RobotsState::Ready(policy);
                true
            }
            Ok(None) => true,
            Err(error) => {
                let has_waiting_request = self.origin_has_live_waiting_request(&origin);
                self.origins
                    .get_mut(&origin)
                    .expect("robots origin exists")
                    .robots = RobotsState::Failed;
                if has_waiting_request {
                    self.emit(Err(error)).await
                } else {
                    true
                }
            }
        }
    }

    async fn handle_page_completion(
        &mut self,
        attempt_url: Url,
        redirects: usize,
        deadline: Instant,
        result: Result<FetchStep>,
    ) -> bool {
        match result {
            Err(error) => self.fail_attempt(attempt_url, error).await,
            Ok(FetchStep::Redirect(mut redirect)) => {
                if redirects >= self.fetch.limits().max_redirects {
                    return self
                        .fail_attempt(attempt_url, Error::Policy("redirect limit exceeded".into()))
                        .await;
                }
                let target = match normalize_url(&redirect.next_request.url) {
                    Ok(target) => target,
                    Err(error) => return self.fail_attempt(attempt_url, error).await,
                };
                if is_destructive_url(&target) {
                    return self
                        .fail_attempt(
                            attempt_url,
                            Error::Policy("destructive redirect target is not permitted".into()),
                        )
                        .await;
                }
                if self.config.same_origin_only
                    && !within_origin_scope(
                        &self.config.start_url,
                        &target,
                        self.config.include_subdomains,
                    )
                {
                    return self
                        .fail_attempt(
                            attempt_url,
                            Error::Policy("redirect target is outside crawl origin scope".into()),
                        )
                        .await;
                }
                if !self.claim_redirect_target(&target) {
                    return self.skip_converged_attempt(attempt_url);
                }
                redirect.next_request.url = target;
                self.frontier.push_front(FrontierItem::Redirect {
                    attempt_url,
                    redirects: redirects + 1,
                    deadline,
                    request: redirect.next_request,
                });
                true
            }
            Ok(FetchStep::Response(page)) => self.finish_page(attempt_url, page).await,
        }
    }

    async fn finish_page(&mut self, attempt_url: Url, page: Page) -> bool {
        self.completed.insert(attempt_url);
        let final_url = match normalize_url(&page.url) {
            Ok(url) => url,
            Err(error) => {
                self.stats.update(|snapshot| snapshot.failed += 1);
                return self.emit(Err(error)).await;
            }
        };
        self.completed.insert(final_url.clone());
        if self.seen.len() < MAX_SEEN_URLS {
            self.seen.insert(final_url.clone());
        }
        self.stats.update(|snapshot| snapshot.succeeded += 1);
        let links = if page_can_contribute_links(&page) {
            self.discover_links(&final_url, &page.html)
        } else {
            Vec::new()
        };
        self.emit(Ok(CrawlResult {
            url: final_url,
            status: page.status,
            html: page.html,
            links,
        }))
        .await
    }

    async fn fail_attempt(&mut self, attempt_url: Url, error: Error) -> bool {
        self.completed.insert(attempt_url);
        self.stats.update(|snapshot| snapshot.failed += 1);
        self.emit(Err(error)).await
    }

    fn claim_redirect_target(&mut self, target: &Url) -> bool {
        if self.owned.contains(target) || self.completed.contains(target) {
            return false;
        }
        if !self.seen.contains(target) && self.seen.len() == MAX_SEEN_URLS {
            return false;
        }
        self.seen.insert(target.clone());
        self.owned.insert(target.clone())
    }

    fn skip_converged_attempt(&mut self, attempt_url: Url) -> bool {
        self.completed.insert(attempt_url);
        self.increment_skipped();
        true
    }

    async fn reject_item(&mut self, item: FrontierItem, origin_failed: bool) -> bool {
        self.increment_skipped();
        match item {
            FrontierItem::Page(reservation)
                if !origin_failed && reservation.url == self.config.start_url =>
            {
                self.emit(Err(Error::RobotsDenied(reservation.url))).await
            }
            FrontierItem::Page(_) => true,
            FrontierItem::Redirect {
                attempt_url,
                request,
                ..
            } => {
                let is_start_attempt = attempt_url == self.config.start_url;
                self.completed.insert(attempt_url);
                if origin_failed {
                    self.stats.update(|snapshot| snapshot.failed += 1);
                    true
                } else if is_start_attempt {
                    self.stats.update(|snapshot| snapshot.failed += 1);
                    self.emit(Err(Error::RobotsDenied(request.url))).await
                } else {
                    true
                }
            }
            FrontierItem::RobotsRedirect { .. } => {
                unreachable!("robots redirects bypass robots policy rejection")
            }
        }
    }

    fn discover_links(&mut self, final_url: &Url, html: &str) -> Vec<Url> {
        let document = Html::parse_document(html);
        let selector = Selector::parse("a[href]").expect("static selector is valid");
        let mut discovered = Vec::with_capacity(MAX_LINKS_PER_PAGE);
        let mut page_identity = HashSet::with_capacity(MAX_LINKS_PER_PAGE);
        for element in document.select(&selector) {
            if discovered.len() == MAX_LINKS_PER_PAGE {
                self.increment_skipped();
                continue;
            }
            let Some(reference) = element.value().attr("href") else {
                continue;
            };
            let candidate = match resolve_and_normalize(final_url, reference) {
                Ok(candidate) => candidate,
                Err(_) => {
                    self.increment_skipped();
                    continue;
                }
            };
            if is_destructive_url(&candidate)
                || (self.config.same_origin_only
                    && !within_origin_scope(
                        &self.config.start_url,
                        &candidate,
                        self.config.include_subdomains,
                    ))
            {
                self.increment_skipped();
                continue;
            }
            if !page_identity.insert(candidate.clone()) {
                continue;
            }
            discovered.push(candidate.clone());
            self.admit_candidate(candidate);
        }
        discovered
    }

    fn admit_candidate(&mut self, candidate: Url) {
        if self.seen.contains(&candidate) {
            return;
        }
        if self.seen.len() == MAX_SEEN_URLS {
            self.increment_skipped();
            return;
        }
        self.seen.insert(candidate.clone());
        if self.cached_policy_denies(&candidate) {
            self.increment_skipped();
            return;
        }
        if self.has_page_reservation() {
            let reservation = self.reserve_page(candidate);
            self.frontier.push_back(FrontierItem::Page(reservation));
            self.stats.update(|snapshot| snapshot.queued += 1);
        } else if self.pending_discoveries.len() < MAX_PENDING_DISCOVERIES {
            self.pending_discoveries.push_back(candidate);
        } else {
            self.increment_skipped();
        }
    }

    fn promote_pending_discoveries(&mut self) {
        while self.has_page_reservation() {
            let Some(candidate) = self.pending_discoveries.pop_front() else {
                break;
            };
            if self.owned.contains(&candidate) {
                continue;
            }
            if self.completed.contains(&candidate) || self.cached_policy_denies(&candidate) {
                self.increment_skipped();
                continue;
            }
            let reservation = self.reserve_page(candidate);
            self.frontier.push_back(FrontierItem::Page(reservation));
            self.stats.update(|snapshot| snapshot.queued += 1);
        }
    }

    fn reserve_page(&mut self, url: Url) -> PageReservation {
        self.owned.insert(url.clone());
        PageReservation { url, proxy: None }
    }

    fn prepare_page_for_schedule(&mut self, item: &mut FrontierItem) {
        let FrontierItem::Page(reservation) = item else {
            return;
        };
        let expected = proxy_for_index(&self.config.proxies, self.scheduled_pages);
        if reservation.proxy.is_some() {
            assert!(
                reservation.proxy == expected,
                "policy admission proxy invariant was violated"
            );
        } else {
            reservation.proxy = expected;
        }
    }

    fn release_page_policy_owner(&mut self, item: &FrontierItem) {
        if matches!(item, FrontierItem::Page(reservation) if self.page_policy_owner.as_ref() == Some(&reservation.url))
        {
            self.page_policy_owner = None;
        }
    }

    fn discard_pending_discoveries(&mut self) {
        let discarded = self.pending_discoveries.len() as u64;
        self.pending_discoveries.clear();
        self.stats.update(|snapshot| snapshot.skipped += discarded);
    }

    fn has_page_reservation(&self) -> bool {
        let reserved = self
            .frontier
            .iter()
            .filter(|item| item.reserves_page())
            .count();
        self.scheduled_pages + reserved < self.config.max_pages
    }

    fn cached_policy_denies(&self, candidate: &Url) -> bool {
        if !self.config.respect_robots {
            return false;
        }
        let Ok(origin) = Origin::from_url(candidate) else {
            return true;
        };
        self.origins
            .get(&origin)
            .is_some_and(|state| match &state.robots {
                RobotsState::Ready(policy) => !policy.allows(candidate),
                RobotsState::Failed => true,
                RobotsState::Unchecked | RobotsState::Fetching => false,
            })
    }

    fn origin_has_live_waiting_request(&self, origin: &Origin) -> bool {
        let now = Instant::now();
        self.frontier.iter().any(|item| match item {
            FrontierItem::Page(_) => {
                Origin::from_url(item.url()).is_ok_and(|candidate| candidate.eq(origin))
            }
            FrontierItem::Redirect { deadline, .. } if *deadline > now => {
                Origin::from_url(item.url()).is_ok_and(|candidate| candidate.eq(origin))
            }
            FrontierItem::Redirect { .. } => false,
            FrontierItem::RobotsRedirect { .. } => false,
        })
    }

    async fn emit(&mut self, item: Result<CrawlResult>) -> bool {
        let sent = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => false,
            sent = self.output.send(item) => sent.is_ok(),
        };
        if !sent {
            self.cancellation.cancel();
        }
        sent
    }

    async fn reap_cancelled(&mut self) {
        self.cancellation.cancel();
        while self.in_flight.next().await.is_some() {}
    }

    fn increment_skipped(&self) {
        self.stats.update(|snapshot| snapshot.skipped += 1);
    }
}

#[derive(Clone, Copy)]
enum RobotsAction {
    Fetch,
    Wait,
    Deny { failed: bool },
    Ready(Option<Duration>),
}

fn crawler_request(url: &Url) -> Result<FetchRequest> {
    let mut request = FetchRequest::request(url.as_str())?;
    request
        .headers
        .insert(USER_AGENT, HeaderValue::from_static(CRAWLER_USER_AGENT));
    Ok(request)
}

fn proxy_for_index(proxies: &[Url], index: usize) -> Option<Url> {
    if proxies.is_empty() {
        None
    } else {
        Some(proxies[index % proxies.len()].clone())
    }
}

fn min_instant(current: Option<Instant>, candidate: Option<Instant>) -> Option<Instant> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.min(candidate)),
        (Some(current), None) => Some(current),
        (None, candidate) => candidate,
    }
}

fn page_can_contribute_links(page: &Page) -> bool {
    (200..300).contains(&page.status)
        && page.content_type.as_deref().is_some_and(|content_type| {
            let media_type = content_type.split(';').next().unwrap_or_default().trim();
            media_type.eq_ignore_ascii_case("text/html")
                || media_type.eq_ignore_ascii_case("application/xhtml+xml")
        })
}

fn validate_config(config: &CrawlConfig) -> Result<()> {
    if !(1..=MAX_PAGES).contains(&config.max_pages) {
        return Err(Error::InvalidInput(format!(
            "max_pages must be between 1 and {MAX_PAGES}"
        )));
    }
    if !(1..=MAX_CONCURRENCY).contains(&config.concurrency) {
        return Err(Error::InvalidInput(format!(
            "concurrency must be between 1 and {MAX_CONCURRENCY}"
        )));
    }
    normalize_url(&config.start_url)?;
    if is_destructive_url(&config.start_url) {
        return Err(Error::Policy(
            "destructive crawl start URL is not permitted".into(),
        ));
    }
    if Instant::now().checked_add(config.minimum_delay).is_none() {
        return Err(Error::InvalidInput("minimum_delay is too large".into()));
    }
    if config.proxies.len() > MAX_PROXIES {
        return Err(Error::InvalidInput(format!(
            "at most {MAX_PROXIES} proxies are permitted"
        )));
    }
    Ok(())
}
