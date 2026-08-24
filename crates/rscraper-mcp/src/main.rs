//! rScrapper MCP server — exposes scrape / search as tools to AI coding agents
//! (Claude Desktop, Cursor, etc.) over the Model Context Protocol on stdio.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout.

use anyhow::Result;
use rscraper_core::fetch::{self, FetchMode, FetchOptions};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                send(&mut stdout, &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } })).await?;
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");

        // Notifications (no id) don't get a response.
        if id.is_none() {
            continue;
        }

        let result: Value = match method {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "rscraper-mcp", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "tools": {} }
            }),

            "tools/list" => json!({
                "tools": [
                    {
                        "name": "scrape",
                        "description": "Fetch a URL and return clean, LLM-ready Markdown (ads/nav stripped). Handles JS-heavy pages via headless browser fallback.",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "url": { "type": "string" } },
                            "required": ["url"]
                        }
                    },
                    {
                        "name": "search",
                        "description": "Web search (DuckDuckGo) returning top results; optionally scrape+clean each page.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string" },
                                "n": { "type": "integer", "default": 5 },
                                "scrape": { "type": "boolean", "default": false }
                            },
                            "required": ["query"]
                        }
                    }
                ]
            }),

            "tools/call" => match handle_tool(&req["params"]).await {
                Ok(text) => json!({ "content": [ { "type": "text", "text": text } ] }),
                Err(e) => json!({ "content": [ { "type": "text", "text": format!("error: {e}") } ], "isError": true }),
            },

            _ => {
                send(&mut stdout, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {method}") } })).await?;
                continue;
            }
        };

        send(&mut stdout, &json!({ "jsonrpc": "2.0", "id": id, "result": result })).await?;
    }

    Ok(())
}

async fn handle_tool(params: &Value) -> Result<String> {
    let name = params["name"].as_str().unwrap_or("");
    match name {
        "scrape" => {
            let url = params["arguments"]["url"].as_str().ok_or_else(|| anyhow::anyhow!("missing `url`"))?;
            let page = fetch::fetch(url, &FetchOptions::new().mode(FetchMode::Auto)).await?;
            Ok(rscraper_core::html_to_markdown(&page.html))
        }
        "search" => {
            let query = params["arguments"]["query"].as_str().ok_or_else(|| anyhow::anyhow!("missing `query`"))?;
            let n = params["arguments"]["n"].as_u64().unwrap_or(5) as usize;
            let scrape = params["arguments"]["scrape"].as_bool().unwrap_or(false);

            let client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (X11; Linux x86_64) Chrome/124.0.0.0 Safari/537.36")
                .build()?;
            let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
            let html = client.get(&url).send().await?.text().await?;

            let doc = scraper::Html::parse_fragment(&html);
            let a_sel = scraper::Selector::parse("a.result__a").unwrap();
            let mut out = String::new();
            for (i, el) in doc.select(&a_sel).enumerate() {
                if i >= n {
                    break;
                }
                let href = el.value().attr("href").unwrap_or_default();
                let real = unwrap_ddg(href);
                let title: String = el.text().collect::<String>();
                out.push_str(&format!("{}. {}\n   {}\n", i + 1, title, real));
                if scrape {
                    if let Ok(page) = fetch::fetch(&real, &FetchOptions::new().mode(FetchMode::Auto)).await {
                        out.push('\n');
                        out.push_str(&rscraper_core::html_to_markdown(&page.html).lines().take(40).collect::<Vec<_>>().join("\n"));
                    }
                }
            }
            Ok(out)
        }
        other => Err(anyhow::anyhow!("unknown tool `{other}`")),
    }
}

async fn send<W: AsyncWriteExt + Unpin>(mut w: W, v: &Value) -> Result<()> {
    let mut line = serde_json::to_string(v)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn unwrap_ddg(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let rest = &href[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    href.to_string()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
