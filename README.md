# rScrapper 🔎

**Free internet access for your coding agent.** A fast, privacy-first Rust toolkit that turns any URL into clean, LLM-ready Markdown — with **no paid APIs**, built-in fallbacks, and first-class support for AI agents (CLI, HTTP API, MCP server).

```
  web page ──▶ rScrapper ──▶ clean Markdown ──▶ your LLM / agent
  YouTube  ──▶ subtitles/transcript
  RSS/Atom ──▶ structured items
  GitHub   ──▶ repos / READMEs / issues (no auth)
  social*  ──▶ Twitter, Reddit, Bilibili, Xiaohongshu, LinkedIn (*optional cookies)
```

---

## Why rScrapper?

| | |
|---|---|
| 🆓 **No paid APIs** | Uses public endpoints (DuckDuckGo, Bing, YouTube, GitHub, Bilibili…) + optional local cookies. No keys required for the core. |
| 🤖 **Agent-friendly** | One smart `get` command auto-routes (page / video / feed / search). Every subcommand is self-documenting with examples — nothing to memorize. `--json` everywhere for scripts. |
| 🔒 **Privacy-first** | Cookies live in `~/.rscraper/` and are only ever sent to the platform you're reading from. Nothing phones home. |
| 🛡️ **Resilient** | Where one route can break, a fallback is built in (DuckDuckGo → Bing; plain HTTP → headless browser for JS/bot-protected pages). |
| ⚡ **Fast & small** | Written in Rust. One static binary per tool. `cargo install` and go. |

---

## What's inside (workspace crates)

| Crate | Binary / lib | Purpose |
|-------|--------------|---------|
| [`rscraper-core`](crates/rscraper-core) | library | The engine: fetching (`Request`/`Js`/`Stealth`/`Auto`), HTML→Markdown, selector memory (re-find elements after redesigns), and a concurrent spider. |
| [`rscraper-cli`](crates/rscraper-cli) | `rscraper` | Agent-facing CLI — read pages, search the web, YouTube/GitHub/RSS, optional social platforms, and a `doctor` health check. |
| [`rscraper-api`](crates/rscraper-api) | `rscraper-api` | Self-hosted HTTP service: `/scrape`, `/search`, `/crawl`. |
| [`rscraper-mcp`](crates/rscraper-mcp) | `rscraper-mcp` | Model Context Protocol server (stdio) exposing `scrape` + `search` tools to Claude Desktop, Cursor, etc. |
| [`robin`](crates/robin) | `robin` | AI-powered **dark web OSINT** research over Tor: LLM refines the query → searches `.onion` engines → filters hits → writes a saved report. |

---

## Installation

### Prerequisites
- [Rust](https://rustup.rs/) (stable, 1.75+) — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- *(optional)* A Chromium-based browser for JS-heavy / bot-protected pages: `sudo apt install chromium` or Google Chrome. Without it, rScrapper still works via plain HTTP and just skips the JS fallback.

### Option 1 — Install from this repo (recommended)
```bash
git clone https://github.com/antarikshkarmakar/rScrapper.git
cd rScrapper
cargo install --path crates/rscraper-cli   # installs `rscraper`
# optional extras:
cargo install --path crates/rscraper-api   # installs `rscraper-api`
cargo install --path crates/rscraper-mcp   # installs `rscraper-mcp`
cargo install --path crates/robin          # installs `robin`

rscraper doctor     # verify your setup
```

### Option 2 — Build from source (dev)
```bash
git clone https://github.com/antarikshkarmakar/rScrapper.git && cd rScrapper
cargo build --release
# binaries land in target/release/: rscraper, rscraper-api, rscraper-mcp, robin
```

### Verify
```bash
rscraper doctor        # what works / what's broken / how to fix it
rscraper               # prints the full cheat-sheet of commands + examples
```

---

## Usage — CLI (`rscraper`)

Run `rscraper` with no args (or `rscraper help`) for the built-in cheat-sheet. Add `--json` to any command for machine-readable output.

### Smart router & reading
```bash
rscraper get "https://example.com"          # auto-detects: page / video / feed / search
rscraper read https://news.ycombinator.com  # web page → clean Markdown (ads/nav stripped)
rscraper search "rust async runtime" -n 10  # web search (DuckDuckGo + Bing fallback)
rscraper search "tokio docs" --scrape       # …and clean each result page into Markdown
```

### YouTube, GitHub, RSS (no API key needed)
```bash
rscraper youtube subs dQw4w9WgXcQ           # subtitles/transcript for a video (URL or ID)
rscraper youtube search "rustconf 2024"     # search videos

rscraper github repo rust-lang/rust         # repo metadata (stars, language, description)
rscraper github readme tokio-rs/tokio       # clean README as Markdown
rscraper github issues rust-lang/rust -n 5  # recent open issues

rscraper rss https://feeds.feedburner.com/rss/...   # RSS/Atom → structured items
```

### Optional social platforms (local cookies)
Reddit and Bilibili work **without login**. Twitter, Xiaohongshu, and LinkedIn need a cookie file. `setup` walks you through it step by step:
```bash
rscraper setup twitter        # guided instructions + where to save the cookie
rscraper social reddit "rust" -n 5
rscraper social bilibili "programming" -n 5
rscraper social twitter "tokio"      # uses ~/.rscraper/twitter.cookies.txt
```

### Diagnostics
```bash
rscraper doctor               # network, browser, state dir, cookies — with fix hints
```

---

## Usage — HTTP API (`rscraper-api`)

A tiny self-hosted service that turns URLs into LLM-ready text. No framework, no DB.

```bash
PORT=8787 rscraper-api        # listens on 0.0.0.0:8787 (override with $PORT)
```

| Method | Path | Body | Returns |
|--------|------|------|---------|
| `GET`  | `/health` | — | liveness |
| `POST` | `/scrape` | `{ "url": "https://…" }` | clean Markdown + status |
| `POST` | `/search` | `{ "query": "…", "n": 5, "scrape": false }` | results (+ optional cleaned pages) |
| `POST` | `/crawl` | `{ "start_url": "https://…", "max_pages": 20 }` | all crawled pages as Markdown |

```bash
curl -s http://localhost:8787/health
curl -s -X POST http://localhost:8787/scrape \
     -H 'Content-Type: application/json' \
     -d '{"url":"https://example.com"}'
```

---

## Usage — MCP server (`rscraper-mcp`)

Exposes `scrape` and `search` as tools to any MCP client (Claude Desktop, Cursor, …) over stdio. Add this to your MCP config:

```json
{
  "mcpServers": {
    "rscraper": {
      "command": "/path/to/rscraper-mcp"
    }
  }
}
```

Tools provided:
- **`scrape`** — `{ "url": "…" }` → clean, LLM-ready Markdown (JS/bot fallback included).
- **`search`** — `{ "query": "…", "n": 5, "scrape": false }` → top results; set `scrape: true` to also clean each page.

---

## Usage — Robin (dark web OSINT)

AI-powered dark web research over Tor, inspired by NetworkChuck's guide. Pipeline: **LLM refines query → search `.onion` engines via Tor → LLM filters relevant hits → LLM writes a summary → report saved to disk.** Captcha/Cloudflare pages are retried with a headless browser.

```bash
robin "what is being discussed about <topic> on the dark web" \
     --provider ollama --model llama3 --save reports/

# or use any LLM you already have keys for:
robin "query" --provider openai  --model gpt-4o-mini
robin "query" --provider claude  --model claude-3-5-haiku-latest
robin "query" --provider gemini  --model gemini-1.5-flash

robin --interactive              # step-by-step prompts (choose provider, enter query)
```

> **Tor:** Robin routes through `socks5://127.0.0.1:9050` by default — start Tor first (`tor`). Override with `--tor socks5h://host:port`.
> **LLM keys:** set the matching env var for your provider — `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`. Ollama needs no key (local, default at `http://127.0.0.1:11434`).

---

## Using it as a library (`rscraper-core`)

```toml
[dependencies]
rscraper-core = { path = "crates/rscraper-core" }   # or git/registry once published
tokio = { version = "1", features = ["rt-multi-thread"] }
```

```rust
use rscraper_core::{fetch, FetchMode, FetchOptions, html_to_markdown};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Auto mode: plain HTTP first, headless browser fallback for JS/bot pages.
    let page = fetch("https://example.com", &FetchOptions::new().mode(FetchMode::Auto)).await?;

    println!("{}", html_to_markdown(&page.html));   // clean Markdown, ads/nav stripped
    Ok(())
}
```

Key exports: `fetch`, `FetchMode` (`Request`/`Js`/`Stealth`/`Auto`), `Page`, `html_to_markdown`, `SelectorMemory` (re-find elements after a site redesigns), and the concurrent spider (`crawl_stream` / `crawl_collect`).

---

## Configuration & environment variables

| Variable | Purpose |
|----------|---------|
| `RSCRAPER_HOME` | Override state dir (default `~/.rscraper`) — where cookie files live. |
| `PORT` | Port for `rscraper-api` (default `8787`). |
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` | LLM provider keys for `robin`. |
| `OLLAMA_HOST` | Ollama endpoint (default `http://127.0.0.1:11434`). |

Cookie files (only sent to their own platform): `~/.rscraper/{twitter,reddit,xiaohongshu,linkedin}.cookies.txt`. Use `rscraper setup <platform>` for exact steps.

---

## Development

```bash
cargo build --workspace     # compile all 5 crates
cargo test  --workspace     # run the full test suite (core + CLI)
cargo clippy --workspace    # lints
```

Project layout:
```
rScrapper/
├── Cargo.toml                 # workspace root
└── crates/
    ├── rscraper-core/         # fetch · markdown · selectors · spider  (library)
    ├── rscraper-cli/          # `rscraper` — web/youtube/github/rss/social/setup/doctor
    ├── rscraper-api/          # `rscraper-api` — /scrape /search /crawl
    ├── rscraper-mcp/          # `rscraper-mcp` — MCP tools over stdio
    └── robin/                 # `robin` — dark web OSINT over Tor + LLM
```

---

## License

MIT © antarikshkarmakar
