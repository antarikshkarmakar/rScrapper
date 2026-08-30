//! # rscraper-core
//!
//! The policy and parsing engine behind rScrapper 0.2: bounded HTTP/browser
//! fetching, CSS and a documented XPath-style selector subset, bounded
//! HTML-to-Markdown conversion, and a typed concurrent crawler.
//!
//! Remote content is untrusted data. [`FetchClient`] applies its
//! [`NetworkPolicy`] during URL validation, DNS resolution, redirects, proxy
//! selection, and browser interception. [`OperationLimits`] bounds transport
//! and output work. Callers still decide whether a target is authorized and
//! how untrusted output may be used.
//!
//! # Typed fetch
//!
//! ```no_run
//! use rscraper_core::{FetchClient, FetchRequest};
//!
//! # async fn example() -> rscraper_core::Result<()> {
//! let client = FetchClient::builder().build()?;
//! let page = client
//!     .fetch_request(FetchRequest::auto(concat!("https", "://example.com"))?)
//!     .await?;
//! assert!(page.url.has_host());
//! # Ok(())
//! # }
//! ```
//!
//! The temporary 0.1 free-function facade is intentionally absent:
//!
//! ```compile_fail
//! use rscraper_core::{fetch, FetchOptions};
//! ```

pub mod browser;
mod browser_cdp;
pub mod client;
pub mod document;
pub mod error;
pub mod limits;
pub mod markdown;
mod mime_policy;
pub mod policy;
pub mod robots;
pub mod selectors;
pub mod spider;
pub mod urlnorm;

// Re-export the most-used items for ergonomic `use rscraper_core::...`.
pub use browser::{looks_like_javascript_shell, BrowserBackend, BrowserEgress, BrowserRenderer};
pub use client::{
    FetchClient, FetchClientBuilder, FetchHostRestriction, FetchMode, FetchRedirect, FetchRequest,
    FetchStep, RawResponse, RobotsFetchStep,
};
pub use document::{FetchVia, Page};
pub use error::{Error, Result};
pub use limits::{truncate_chars, OperationLimits};
pub use markdown::html_to_markdown;
pub use policy::{NetworkPolicy, ResolverSource};
pub use selectors::{clean_text, Fingerprint, Sel, SelectorMemory};
pub use spider::{
    CrawlConfig, CrawlControl, CrawlResult, CrawlStats, CrawlStatsSnapshot, Crawler,
    CRAWLER_USER_AGENT, MAX_LINKS_PER_PAGE,
};

/// Library version (kept in sync with the workspace).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
