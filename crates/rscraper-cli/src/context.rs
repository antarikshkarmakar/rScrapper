use anyhow::Result;
use rscraper_core::{BrowserEgress, BrowserRenderer, FetchClient, NetworkPolicy};
use std::path::PathBuf;
use std::sync::Arc;

/// Shared policy and state services for CLI-derived operations.
#[derive(Clone)]
pub struct AppContext {
    /// Reusable policy-enforcing fetch client.
    pub fetch: FetchClient,
    /// Discovered browser renderer, when a supported executable is available.
    pub browser: Option<Arc<BrowserRenderer>>,
    /// Private local state directory.
    pub config_dir: PathBuf,
}

impl AppContext {
    /// Construct the normal public-network context.
    pub fn try_default() -> Result<Self> {
        let browser =
            BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::PublicInternet))
                .ok()
                .map(Arc::new);
        let fetch = match &browser {
            Some(renderer) => {
                let backend: Arc<dyn rscraper_core::BrowserBackend> = renderer.clone();
                FetchClient::builder().browser(backend).build()?
            }
            None => FetchClient::builder().build()?,
        };

        Ok(Self {
            fetch,
            browser,
            config_dir: config_dir(),
        })
    }

    /// Construct the explicit private-network context used only by `doctor`
    /// for its loopback request, TLS, and optional browser fixtures.
    pub fn try_diagnostic() -> Result<Self> {
        Self::try_diagnostic_at(config_dir())
    }

    #[doc(hidden)]
    pub fn try_diagnostic_at(config_dir: PathBuf) -> Result<Self> {
        let browser = BrowserRenderer::discover(BrowserEgress::Direct(NetworkPolicy::AllowPrivate))
            .ok()
            .map(Arc::new);
        let builder = FetchClient::builder().policy(NetworkPolicy::AllowPrivate);
        let fetch = match &browser {
            Some(renderer) => {
                let backend: Arc<dyn rscraper_core::BrowserBackend> = renderer.clone();
                builder.browser(backend).build()?
            }
            None => builder.build()?,
        };
        Ok(Self {
            fetch,
            browser,
            config_dir,
        })
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("RSCRAPER_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".rscraper")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rscraper_core::NetworkPolicy;

    #[test]
    fn diagnostic_context_is_explicitly_private_enabled_without_changing_default_policy() {
        let diagnostic = AppContext::try_diagnostic_at(PathBuf::from("diagnostic-state")).unwrap();
        assert_eq!(diagnostic.fetch.policy(), NetworkPolicy::AllowPrivate);
        assert_eq!(diagnostic.config_dir, PathBuf::from("diagnostic-state"));

        let normal = AppContext::try_default().unwrap();
        assert_eq!(normal.fetch.policy(), NetworkPolicy::PublicInternet);
    }
}
