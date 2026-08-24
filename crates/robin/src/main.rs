//! Robin CLI — dark web OSINT research with AI refine/filter/summarize.
//!
//! Usage:
//!   robin "query" --provider ollama --model llama3 --save reports/
//!   robin "query" --provider openai --model gpt-4o-mini
//!   robin --interactive          # step-by-step prompts (like the web UI)

use anyhow::Result;
use clap::{Parser, ValueEnum};
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "robin", version, about = "AI-powered dark web OSINT research")]
struct Cli {
    /// The investigation query.
    #[arg(long)]
    query: Option<String>,

    /// LLM provider for refine/filter/summarize.
    #[arg(long, default_value = "ollama")]
    provider: ProviderArg,

    /// Model name (provider-specific).
    #[arg(long, default_value = "llama3")]
    model: String,

    /// Save the report to this directory (creates robin-report-<ts>.md).
    #[arg(long)]
    save: Option<PathBuf>,

    /// Tor SOCKS5 proxy (default http://127.0.0.1:9050 if `tor` is running).
    #[arg(long, default_value = "socks5://127.0.0.1:9050")]
    tor: String,

    /// Step-by-step interactive mode (choose provider, enter query at the prompt).
    #[arg(long)]
    interactive: bool,
}

#[derive(ValueEnum, Clone)]
enum ProviderArg {
    Ollama,
    Openai,
    Claude,
    Gemini,
}

impl From<ProviderArg> for robin::Provider {
    fn from(a: ProviderArg) -> Self {
        match a {
            ProviderArg::Ollama => robin::Provider::Ollama { model: "llama3".into() },
            ProviderArg::Openai => robin::Provider::OpenAI { model: "gpt-4o-mini".into() },
            ProviderArg::Claude => robin::Provider::Claude { model: "claude-3-5-haiku-latest".into() },
            ProviderArg::Gemini => robin::Provider::Gemini { model: "gemini-1.5-flash".into() },
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve query (interactive if not provided).
    let query = match &cli.query {
        Some(q) => q.clone(),
        None => read_line("Enter your dark web investigation query: "),
    };
    if query.trim().is_empty() {
        anyhow::bail!("empty query");
    }

    // Resolve provider + model.
    let mut provider = robin::Provider::from(cli.provider.clone());
    match &mut provider {
        robin::Provider::Ollama { model } => *model = cli.model.clone(),
        robin::Provider::OpenAI { model } => *model = if cli.model == "llama3" { "gpt-4o-mini".into() } else { cli.model.clone() },
        robin::Provider::Claude { model } => *model = if cli.model == "llama3" { "claude-3-5-haiku-latest".into() } else { cli.model.clone() },
        robin::Provider::Gemini { model } => *model = if cli.model == "llama3" { "gemini-1.5-flash".into() } else { cli.model.clone() },
    }

    println!("🔎 Robin — dark web OSINT");
    println!("   query:    {query}");
    println!("   provider: {:?}", provider_name(&provider));
    println!();

    let report = robin::investigate(&query, provider, Some(cli.tor.clone())).await?;

    // Print the summary + sources.
    println!("{}", report.to_markdown());

    // Save if requested (or always to ./reports by default).
    let dir = cli.save.unwrap_or_else(|| PathBuf::from("reports"));
    let path = report.save(&dir.display().to_string())?;
    println!("\n✅ Report saved: {}", path.display());

    Ok(())
}

fn provider_name(p: &robin::Provider) -> &'static str {
    match p {
        robin::Provider::Ollama { .. } => "ollama",
        robin::Provider::OpenAI { .. } => "openai",
        robin::Provider::Claude { .. } => "claude",
        robin::Provider::Gemini { .. } => "gemini",
    }
}

fn read_line(prompt: &str) -> String {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok();
    s.trim().to_string()
}
