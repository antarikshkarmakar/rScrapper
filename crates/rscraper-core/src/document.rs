use std::fmt;
use url::Url;

/// Transport that produced a [`Page`] or raw response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchVia {
    /// Bounded HTTP request transport.
    Request,
    /// Isolated Chromium renderer.
    Browser,
    /// Deterministic injected test transport.
    Test,
}

/// Decoded document response.
///
/// Non-success status codes remain visible to callers. Debug output redacts URL,
/// content-type value, and HTML.
#[derive(Clone)]
pub struct Page {
    /// Final validated URL after redirects.
    pub url: Url,
    /// HTTP or best available main-document status.
    pub status: u16,
    /// Parsed content type when present.
    pub content_type: Option<String>,
    /// Decoded bounded document text.
    pub html: String,
    /// Transport used for the final response.
    pub via: FetchVia,
}

impl fmt::Debug for Page {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Page")
            .field("url", &Redacted)
            .field("status", &self.status)
            .field("content_type_present", &self.content_type.is_some())
            .field("body_len", &self.html.len())
            .field("via", &self.via)
            .finish()
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}
