use anyhow::Result;
use rscraper_api::{
    install_redacted_panic_hook, serve_with_shutdown, validate_server_config, ApiState,
    ServerConfig,
};
use rscraper_cli::context::AppContext;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    initialize_tracing();
    install_redacted_panic_hook();
    let config = ServerConfig::from_env()?;
    validate_server_config(&config)?;
    let context = AppContext::try_default()?;
    let state = ApiState {
        context,
        token: config.token.as_deref().map(Arc::<str>::from),
        operation_limit: Arc::new(Semaphore::new(config.max_concurrent_operations)),
    };
    let listener = TcpListener::bind(config.bind).await?;
    tracing::info!(bind = %config.bind, "rScraper API listening");
    serve_with_shutdown(listener, state, shutdown_signal()).await?;
    Ok(())
}

fn initialize_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            tracing::error!("failed to install Ctrl-C shutdown handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => {
                tracing::error!("failed to install SIGTERM shutdown handler");
                std::future::pending::<()>().await
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
