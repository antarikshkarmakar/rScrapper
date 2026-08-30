use anyhow::Result;
use clap::{Parser, Subcommand};
use rscraper_cli::{context::AppContext, doctor, github, output, rss, social, web, youtube};
use rscraper_core::Error;
use serde_json::json;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "rscraper", version, about = "Secure bounded web scraping services", long_about = None)]
struct Cli {
    /// Emit compact JSON for agents and scripts.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Route a URL/video/feed/query to the matching service.
    Get {
        target: String,
        #[arg(short = 'n', long, default_value_t = web::DEFAULT_SEARCH_RESULTS)]
        n: usize,
    },
    /// Read a web page as bounded Markdown.
    Read { url: String },
    /// Search DuckDuckGo with a diagnostic-preserving Bing fallback.
    Search {
        query: String,
        #[arg(short = 'n', long, default_value_t = web::DEFAULT_SEARCH_RESULTS)]
        n: usize,
        #[arg(long)]
        scrape: bool,
    },
    /// YouTube subtitles and search.
    #[command(subcommand)]
    Youtube(YoutubeCmd),
    /// Public GitHub repository data.
    #[command(subcommand)]
    Github(GithubCmd),
    /// Parse an RSS, Atom, or JSON feed.
    Rss { url: String },
    /// Public and authenticated social-platform adapters.
    #[command(subcommand)]
    Social(SocialCmd),
    /// Create a private platform cookie template without overwriting data.
    Setup { platform: String },
    /// Run deterministic local health checks.
    Doctor {
        /// Opt in to a non-fatal external HTTPS reachability check.
        #[arg(long)]
        live: bool,
    },
}

#[derive(Subcommand)]
enum YoutubeCmd {
    /// Pull subtitles/transcript for a video URL or ID.
    Subs { video: String },
    /// Search YouTube videos.
    Search {
        query: String,
        #[arg(short = 'n', long, default_value_t = 5)]
        n: usize,
    },
}

#[derive(Subcommand)]
enum GithubCmd {
    /// Show public repository metadata.
    Repo { owner_repo: String },
    /// Fetch and decode a repository README.
    Readme { owner_repo: String },
    /// List open issues, excluding pull requests.
    Issues {
        owner_repo: String,
        #[arg(short = 'n', long, default_value_t = 10)]
        n: usize,
    },
}

#[derive(Subcommand)]
enum SocialCmd {
    /// Twitter/X timeline or search using private local cookies.
    Twitter { query: Option<String> },
    /// Public Reddit search.
    Reddit {
        query: String,
        #[arg(short = 'n', long, default_value_t = 10)]
        n: usize,
    },
    /// Public Bilibili video search.
    Bilibili {
        query: String,
        #[arg(short = 'n', long, default_value_t = 5)]
        n: usize,
    },
    /// Xiaohongshu search using private local cookies.
    Xiaohongshu { query: String },
    /// LinkedIn people search using private local cookies.
    Linkedin { query: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    let json_requested = std::env::args_os()
        .skip(1)
        .take_while(|argument| argument != "--")
        .any(|argument| argument == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            if output::emit_cli_parse_error(json_requested, &error).is_err() {
                return ExitCode::FAILURE;
            }
            return if exit_code == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(u8::try_from(exit_code).unwrap_or(1))
            };
        }
    };
    match run(&cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if output::emit_error(cli.json, &error).is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: &Cli) -> Result<()> {
    let Some(command) = &cli.command else {
        return print_cheatsheet(cli.json);
    };

    if let Cmd::Doctor { live } = command {
        let local_context = AppContext::try_diagnostic()?;
        let live_context = if *live {
            Some(AppContext::try_default()?)
        } else {
            None
        };
        let report = doctor::run(
            &local_context,
            live_context.as_ref(),
            doctor::DoctorOptions::standard(*live),
        )
        .await?;
        let human = output::render_doctor(&report);
        return output::emit_value(cli.json, &report, &human);
    }

    let context = AppContext::try_default()?;
    match command {
        Cmd::Get { target, n } => smart_get(&context, target, *n, cli.json).await,
        Cmd::Read { url } => {
            let response = web::read(&context, url).await?;
            let human = output::render_read(&response);
            output::emit_value(cli.json, &response, &human)
        }
        Cmd::Search { query, n, scrape } => {
            let response = web::search(&context, query, *n, *scrape).await?;
            let human = output::render_search(&response);
            output::emit_value(cli.json, &response, &human)
        }
        Cmd::Youtube(command) => match command {
            YoutubeCmd::Subs { video } => {
                youtube::subs_with_context(&context, video, cli.json).await
            }
            YoutubeCmd::Search { query, n } => {
                validate_count(*n, 100, "YouTube result count")?;
                youtube::search_with_context(&context, query, *n, cli.json).await
            }
        },
        Cmd::Github(command) => match command {
            GithubCmd::Repo { owner_repo } => {
                let response = github::repo(&context, owner_repo).await?;
                let human = output::render_repo(&response);
                output::emit_value(cli.json, &response, &human)
            }
            GithubCmd::Readme { owner_repo } => {
                let response = github::readme(&context, owner_repo).await?;
                let human = response.readme.clone();
                output::emit_value(cli.json, &response, &human)
            }
            GithubCmd::Issues { owner_repo, n } => {
                let response = github::issues(&context, owner_repo, *n).await?;
                let human = output::render_issues(&response);
                output::emit_value(cli.json, &response, &human)
            }
        },
        Cmd::Rss { url } => rss::parse_with_context(&context, url, cli.json).await,
        Cmd::Social(command) => match command {
            SocialCmd::Twitter { query } => {
                let response = social::twitter(&context, query.as_deref()).await?;
                let human = response.content.clone();
                output::emit_value(cli.json, &response, &human)
            }
            SocialCmd::Reddit { query, n } => {
                let response = social::reddit(&context, query, *n).await?;
                let human = output::render_reddit(&response);
                output::emit_value(cli.json, &response, &human)
            }
            SocialCmd::Bilibili { query, n } => {
                let response = social::bilibili(&context, query, *n).await?;
                let human = output::render_bilibili(&response);
                output::emit_value(cli.json, &response, &human)
            }
            SocialCmd::Xiaohongshu { query } => {
                let response = social::xiaohongshu(&context, query).await?;
                let human = response.content.clone();
                output::emit_value(cli.json, &response, &human)
            }
            SocialCmd::Linkedin { query } => {
                let response = social::linkedin(&context, query).await?;
                let human = response.content.clone();
                output::emit_value(cli.json, &response, &human)
            }
        },
        Cmd::Setup { platform } => {
            let response = social::setup(&context, platform)?;
            let human = output::render_setup(&response);
            output::emit_value(cli.json, &response, &human)
        }
        Cmd::Doctor { .. } => unreachable!("doctor was handled before default context creation"),
    }
}

async fn smart_get(
    context: &AppContext,
    target: &str,
    count: usize,
    json_output: bool,
) -> Result<()> {
    let target = target.trim();
    if let Some(video_id) = youtube::extract_video_id(target) {
        return youtube::subs_with_context(context, &video_id, json_output).await;
    }
    if is_feed_url(target) || rss::looks_like_feed(target) {
        return rss::parse_with_context(context, target, json_output).await;
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        let response = web::read(context, target).await?;
        let human = output::render_read(&response);
        return output::emit_value(json_output, &response, &human);
    }
    let response = web::search(context, target, count, false).await?;
    let human = output::render_search(&response);
    output::emit_value(json_output, &response, &human)
}

fn validate_count(value: usize, maximum: usize, name: &str) -> Result<()> {
    if !(1..=maximum).contains(&value) {
        return Err(Error::InvalidInput(format!("{name} must be between 1 and {maximum}")).into());
    }
    Ok(())
}

fn is_feed_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("/rss")
        || lower.contains("/feed")
        || lower.ends_with(".xml")
        || lower.contains("atom.xml")
}

fn print_cheatsheet(json_output: bool) -> Result<()> {
    let text = r#"rScrapper — secure bounded internet services

  get <url-or-query>        Smart router: page / YouTube / RSS / search
  read <url>                Web page to bounded Markdown
  search <query> [-n N]     DuckDuckGo search with Bing fallback
  youtube subs <video>      Subtitles/transcript for a video
  youtube search <query>    Search YouTube videos
  github repo <owner/repo>  Repository metadata
  github readme <owner/repo> Decode a README
  github issues <owner/repo> Open issues excluding pull requests
  rss <feed-url>            Parse RSS/Atom/JSON Feed
  social <platform> ...     Reddit, Bilibili, and authenticated adapters
  setup <platform>          Create private cookie setup instructions
  doctor [--live]           Local checks; external reachability is opt-in

Flags: --json   Environment: RSCRAPER_HOME"#;
    output::emit_json_value(json_output, &json!({ "commands": text }), text)
}
