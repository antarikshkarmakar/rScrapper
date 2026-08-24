//! rScrapper CLI — gives a coding agent (or you) free internet access.
//!
//! Design goals:
//! * **No paid APIs.** Everything uses public endpoints + optional local cookies.
//! * **Agent-friendly.** A single smart `get` command auto-routes, and every
//!   subcommand is self-documenting with examples — no memorizing needed.
//! * **Privacy-first.** Cookies live in `~/.rscraper/`, never sent anywhere but
//!   the platform you're reading from.
//! * **Resilient.** Where one route can break, a fallback route is built in.

pub mod doctor;
pub mod github;
pub mod rss;
pub mod social;
pub mod web;
pub mod youtube;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// rScrapper — free internet access for your coding agent.
#[derive(Parser)]
#[command(name = "rscraper", version, about, long_about = None)]
struct Cli {
    /// Emit compact JSON (for agents / scripts) instead of human text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Smart router: give it a URL or a search query and it figures out the rest.
    Get {
        /// A URL (web page / YouTube video / RSS feed) OR a plain search query.
        target: String,
        /// Number of results when this is treated as a search.
        #[arg(short = 'n', long, default_value_t = 5)]
        n: usize,
    },

    /// Read a web page and print clean Markdown (ads/nav stripped).
    Read { url: String },

    /// Search the web (DuckDuckGo, with Bing fallback) and return top results.
    Search {
        query: String,
        #[arg(short = 'n', long, default_value_t = 5)]
        n: usize,
        /// Also scrape + clean each result's page into Markdown.
        #[arg(long)]
        scrape: bool,
    },

    /// YouTube: subtitles and video search (no API key).
    #[command(subcommand)]
    Youtube(YoutubeCmd),

    /// Public GitHub data (repos, READMEs, issues) without auth.
    #[command(subcommand)]
    Github(GithubCmd),

    /// Parse an RSS / Atom feed into clean items.
    Rss { url: String },

    /// Optional social platforms (need local cookies; run `setup` first).
    #[command(subcommand)]
    Social(SocialCmd),

    /// Guided, step-by-step setup for a platform that needs login/cookies.
    Setup { platform: String },

    /// Doctor: check what works, what's broken, and how to fix it.
    Doctor,
}

#[derive(Subcommand)]
enum YoutubeCmd {
    /// Pull subtitles/transcript for a video (URL or ID).
    Subs { video: String },
    /// Search YouTube videos.
    Search { query: String, #[arg(short = 'n', long, default_value_t = 5)] n: usize },
}

#[derive(Subcommand)]
enum GithubCmd {
    /// Show a public repo's metadata (stars, language, description).
    Repo { owner_repo: String },
    /// Fetch and clean a repo's README.
    Readme { owner_repo: String },
    /// List recent issues on a public repo.
    Issues { owner_repo: String, #[arg(short = 'n', long, default_value_t = 10)] n: usize },
}

#[derive(Subcommand)]
enum SocialCmd {
    /// Twitter/X timeline or search (needs cookies).
    Twitter { query: Option<String> },
    /// Reddit public posts/search (works without login; cookie optional).
    Reddit { query: String, #[arg(short = 'n', long, default_value_t = 10)] n: usize },
    /// Bilibili video search (public API, no login needed).
    Bilibili { query: String, #[arg(short = 'n', long, default_value_t = 5)] n: usize },
    /// Xiaohongshu notes/search (needs cookies).
    Xiaohongshu { query: String },
    /// LinkedIn people/company search (needs cookies).
    Linkedin { query: String },
}

/// Where local state (cookies, element memory) lives.
fn config_dir() -> PathBuf {
    std::env::var("RSCRAPER_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_home().join(".rscraper"))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.command.is_none() {
        print_cheatsheet(cli.json);
        return Ok(());
    }

    match &cli.command {
        Some(Cmd::Get { target, n }) => web::smart_get(target, *n, cli.json).await,
        Some(Cmd::Read { url }) => web::read(url, cli.json).await,
        Some(Cmd::Search { query, n, scrape }) => web::search(query, *n, *scrape, cli.json).await,
        Some(Cmd::Youtube(c)) => match c {
            YoutubeCmd::Subs { video } => youtube::subs(video, cli.json).await,
            YoutubeCmd::Search { query, n } => youtube::search(query, *n, cli.json).await,
        },
        Some(Cmd::Github(c)) => match c {
            GithubCmd::Repo { owner_repo } => github::repo(owner_repo, cli.json).await,
            GithubCmd::Readme { owner_repo } => github::readme(owner_repo, cli.json).await,
            GithubCmd::Issues { owner_repo, n } => github::issues(owner_repo, *n, cli.json).await,
        },
        Some(Cmd::Rss { url }) => rss::parse(url, cli.json).await,
        Some(Cmd::Social(c)) => match c {
            SocialCmd::Twitter { query } => social::twitter(query.as_deref(), cli.json).await,
            SocialCmd::Reddit { query, n } => social::reddit(query, *n, cli.json).await,
            SocialCmd::Bilibili { query, n } => social::bilibili(query, *n, cli.json).await,
            SocialCmd::Xiaohongshu { query } => social::xiaohongshu(query, cli.json).await,
            SocialCmd::Linkedin { query } => social::linkedin(query, cli.json).await,
        },
        Some(Cmd::Setup { platform }) => social::setup(platform, cli.json),
        Some(Cmd::Doctor) => doctor::run(cli.json).await,
        None => Ok(()), // handled above (cheatsheet already printed)
    }
}

/// A friendly cheat-sheet so an agent (or human) never has to memorize commands.
fn print_cheatsheet(json: bool) -> Result<()> {
    let text = r#"rScrapper — free internet for your coding agent

  get <url-or-query>        Smart router: page / YouTube / RSS / search, auto-detected
  read <url>                Web page → clean Markdown (ads & nav stripped)
  search <query> [-n N]     Web search (DuckDuckGo + Bing fallback); add --scrape to clean each page
  youtube subs <video>      Subtitles/transcript for a video (URL or ID)
  youtube search <q>        Search YouTube videos
  github repo <owner/repo>  Repo metadata (stars, language, description)
  github readme <o/r>       Clean README
  github issues <o/r>       Recent issues
  rss <feed-url>            Parse RSS/Atom into items

Optional social (run `rscraper setup <platform>` first when cookies are needed):
  reddit <query>            Public posts/search — works without login
  bilibili <query>          Video search — public API, no login
  twitter <query>           Needs cookies
  xiaohongshu <query>       Needs cookies
  linkedin <query>          Needs cookies

Diagnostics:
  doctor                    What works / what's broken / how to fix it
  help                      This cheat-sheet

Flags: --json (machine-readable output)   RSCRAPER_HOME (override state dir, default ~/.rscraper)

Examples:
  rscraper get https://example.com
  rscraper search "rust async runtime" -n 5 --scrape
  rscraper youtube subs dQw4w9WgXcQ
  rscraper github readme tokio-rs/tokio
  rscraper doctor
"#;

    if json {
        println!("{}", serde_json::json!({ "commands": text }));
    } else {
        println!("{text}");
    }
    Ok(())
}
