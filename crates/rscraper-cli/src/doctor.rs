//! `rscraper doctor` — a health check that reports what works, what's broken,
//! and exactly how to fix each problem. This is the "what do I do now?" command.

use anyhow::Result;
use serde_json::json;
use std::path::PathBuf;
use crate::{web, youtube};
use web::http_client;

#[derive(Debug)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn icon(&self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
        }
    }
}

struct Check {
    name: String,
    status: Status,
    detail: String,
    fix: Option<String>,
}

/// GET a URL and return the HTTP status (or an error string).
async fn probe(url: &str) -> Result<u16> {
    let client = http_client()?;
    let resp = client.get(url).send().await?;
    Ok(resp.status().as_u16())
}

/// Run all checks and print a report.
pub async fn run(json_out: bool) -> Result<()> {
    let mut checks = Vec::new();

    // 1) Outbound network (can we reach the web at all?).
    match probe("https://example.com").await {
        Ok(code) if code < 400 => checks.push(Check { name: "network".into(), status: Status::Ok, detail: format!("outbound HTTPS works (HTTP {code})"), fix: None }),
        Ok(code) => checks.push(Check { name: "network".into(), status: Status::Warn, detail: format!("got HTTP {code} from example.com"), fix: Some("check your proxy / firewall settings".into()) }),
        Err(e) => checks.push(Check { name: "network".into(), status: Status::Fail, detail: e.to_string(), fix: Some("no internet access — check connectivity or set a proxy".into()) }),
    }

    // 2) Headless browser (needed for JS-heavy / bot-protected pages).
    match rscraper_core::fetch::find_chromium() {
        Some(bin) => checks.push(Check { name: "browser (JS/stealth)".into(), status: Status::Ok, detail: format!("found `{bin}`"), fix: None }),
        None => checks.push(Check {
            name: "browser (JS/stealth)".into(),
            status: Status::Warn,
            detail: "no chromium/chrome found — JS-heavy pages will fall back to plain requests".into(),
            fix: Some("install one: `sudo apt install chromium` or `google-chrome-stable`".into()),
        }),
    }

    // 3) Local state dir.
    let home = std::env::var("RSCRAPER_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".rscraper"));
    match std::fs::create_dir_all(&home) {
        Ok(_) => checks.push(Check { name: "local state dir".into(), status: Status::Ok, detail: format!("{} (cookies live here)", home.display()), fix: None }),
        Err(e) => checks.push(Check { name: "local state dir".into(), status: Status::Fail, detail: e.to_string(), fix: Some("make sure $HOME is writable".into()) }),
    }

    // 4) Optional platform cookies (informational — only needed if you use them).
    for (platform, cookie_file) in [
        ("twitter", "twitter.cookies.txt"),
        ("reddit", "reddit.cookies.txt"),
        ("xiaohongshu", "xiaohongshu.cookies.txt"),
        ("linkedin", "linkedin.cookies.txt"),
    ] {
        let path = home.join(cookie_file);
        if path.exists() {
            checks.push(Check { name: format!("cookies: {platform}").into(), status: Status::Ok, detail: "present (local)".into(), fix: None });
        } else {
            checks.push(Check {
                name: format!("cookies: {platform}").into(),
                status: Status::Warn,
                detail: "not set — only needed if you use this platform".into(),
                fix: Some(format!("run `rscraper setup {platform}` for step-by-step help")),
            });
        }
    }

    // 5) Bilibili (no login needed) — just confirm the API is reachable.
    match probe("https://api.bilibili.com/x/web-interface/search/type?search_type=video&keyword=test").await {
        Ok(code) => checks.push(Check { name: "bilibili (public)".into(), status: Status::Ok, detail: format!("API reachable (HTTP {code})"), fix: None }),
        Err(e) => checks.push(Check { name: "bilibili (public)".into(), status: Status::Warn, detail: e.to_string(), fix: Some("Bilibili API unreachable from here — may be region-limited".into()) }),
    }

    // Render.
    let mut text = String::from("rScrapper doctor\n");
    let mut all_ok = true;
    for c in &checks {
        if matches!(c.status, Status::Fail) {
            all_ok = false;
        }
        text.push_str(&format!("  {} {}\n", c.status.icon(), c.name));
        text.push_str(&format!("     {}\n", c.detail));
        if let Some(fix) = &c.fix {
            text.push_str(&format!("     fix: {fix}\n"));
        }
    }

    let verdict: &'static str = if all_ok {
        "All core checks passed. Optional platforms are set up as needed."
    } else {
        "Some checks need attention - see the `fix:` lines above."
    };
    text.push('\n');
    text.push_str(verdict);

    let json_val = json!({
        "checks": checks.iter().map(|c| json!({
            "name": c.name,
            "status": match c.status { Status::Ok => "ok", Status::Warn => "warn", Status::Fail => "fail" },
            "detail": c.detail,
            "fix": c.fix,
        })).collect::<Vec<_>>(),
        "all_ok": all_ok,
    });

    web::emit(json_out, &json_val, &text)
}
