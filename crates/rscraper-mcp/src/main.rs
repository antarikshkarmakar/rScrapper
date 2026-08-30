use anyhow::Context as _;
use rmcp::ServiceExt;
use rscraper_cli::context::AppContext;
use rscraper_mcp::{
    init_safe_stderr_tracing, trace_service_starting, GuardedStdioTransport, RscraperMcp,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_safe_stderr_tracing().map_err(|_| anyhow::anyhow!("failed to initialize tracing"))?;

    let context = AppContext::try_default().context("failed to initialize application context")?;
    trace_service_starting();
    RscraperMcp::new(context)
        .serve(GuardedStdioTransport::new(
            tokio::io::stdin(),
            tokio::io::stdout(),
        ))
        .await
        .context("failed to start MCP stdio service")?
        .waiting()
        .await
        .context("MCP stdio service failed")?;
    Ok(())
}
