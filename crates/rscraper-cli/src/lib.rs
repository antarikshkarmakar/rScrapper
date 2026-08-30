//! Reusable platform services for the rScrapper 0.2 CLI, API, and MCP adapters.
//!
//! [`context::AppContext`] owns one policy-enforcing fetch client and optional
//! browser. Service modules return typed, bounded values and treat provider
//! responses as untrusted. [`cookies::PlatformCookieJar`] is origin-scoped and
//! redacted; Unix loaders require regular non-symlink owner-only files.

pub mod context;
pub mod cookies;
pub mod doctor;
pub mod github;
pub mod output;
pub mod rss;
pub mod social;
pub mod web;
pub mod youtube;
