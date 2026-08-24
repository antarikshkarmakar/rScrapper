//! # rscraper-core
//!
//! The engine behind rScrapper: page fetching (request / JS / stealth with
//! automatic fallback), CSS + XPath-style selectors with **smart element
//! memory**, HTML→Markdown conversion, and a concurrent spider.
//!
//! This crate is dependency-light and network-free in its core logic so it can
//! be unit-tested anywhere.

pub mod fetch;
pub mod markdown;
pub mod selectors;
pub mod spider;

// Re-export the most-used items for ergonomic `use rscraper_core::...`.
pub use fetch::{fetch, FetchMode, FetchOptions, Page};
pub use markdown::html_to_markdown;
pub use selectors::{clean_text, Fingerprint, Sel, SelectorMemory};
pub use spider::{crawl_collect, crawl_stream, CrawlResult, CrawlState, SpiderConfig, Stats};

/// Library version (kept in sync with the workspace).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
