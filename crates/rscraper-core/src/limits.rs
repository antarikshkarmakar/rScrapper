use std::time::Duration;

/// Largest supported connection-establishment timeout.
///
/// A fixed bound keeps transport timer construction independent of process
/// uptime while allowing substantially more time than the 10-second default.
pub const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest supported complete-request timeout, including redirects and body.
///
/// A fixed bound keeps absolute deadline construction independent of process
/// uptime and matches the platform's longest public operation budget.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Fixed limits applied by a [`crate::FetchClient`] and related operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationLimits {
    /// Positive connection-establishment timeout, at most [`MAX_CONNECT_TIMEOUT`].
    pub connect_timeout: Duration,
    /// Positive complete-request timeout, at most [`MAX_REQUEST_TIMEOUT`].
    pub request_timeout: Duration,
    /// Maximum streamed, decoded response-body bytes.
    pub max_body_bytes: usize,
    /// Maximum rendered Unicode scalar values.
    pub max_output_chars: usize,
    /// Maximum followed redirect hops.
    pub max_redirects: usize,
}

impl Default for OperationLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_body_bytes: 5 * 1024 * 1024,
            max_output_chars: 1_000_000,
            max_redirects: 10,
        }
    }
}

/// Return at most `limit` Unicode scalar values without splitting UTF-8.
pub fn truncate_chars(input: &str, limit: usize) -> String {
    input.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::{truncate_chars, OperationLimits};
    use std::time::Duration;

    #[test]
    fn operation_limits_have_secure_defaults() {
        let limits = OperationLimits::default();
        assert_eq!(limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(limits.request_timeout, Duration::from_secs(30));
        assert_eq!(limits.max_body_bytes, 5 * 1024 * 1024);
        assert_eq!(limits.max_output_chars, 1_000_000);
        assert_eq!(limits.max_redirects, 10);
    }

    #[test]
    fn truncate_chars_never_splits_unicode() {
        assert_eq!(truncate_chars("a🦀b", 2), "a🦀");
    }
}
