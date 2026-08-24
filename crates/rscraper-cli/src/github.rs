//! Public GitHub data without authentication (REST API + raw README).

use anyhow::{anyhow, Result};
use serde_json::json;
use crate::{web, youtube};
use web::http_client;

/// `owner/repo` → split into owner and repo.
fn parts(owner_repo: &str) -> Result<(String, String)> {
    let mut it = owner_repo.splitn(2, '/');
    match (it.next(), it.next()) {
        (Some(o), Some(r)) if !o.is_empty() && !r.is_empty() => Ok((o.to_string(), r.trim_end_matches('/').to_string())),
        _ => Err(anyhow!("expected `owner/repo`, got `{owner_repo}`")),
    }
}

/// Repo metadata.
pub async fn repo(owner_repo: &str, json_out: bool) -> Result<()> {
    let (o, r) = parts(owner_repo)?;
    let client = http_client()?;
    let url = format!("https://api.github.com/repos/{o}/{r}");
    let v: serde_json::Value = client.get(&url).send().await?.json().await?;

    if v.get("message").is_some() && v.get("full_name").is_none() {
        return Err(anyhow!("GitHub said: {}", v["message"].as_str().unwrap_or("not found")));
    }

    let out = json!({
        "name": v["full_name"],
        "description": v["description"],
        "stars": v["stargazers_count"],
        "forks": v["forks_count"],
        "language": v["language"],
        "license": v.get("license").and_then(|l| l.get("spdx_id")).map(|x| x.to_string()),
        "open_issues": v["open_issues_count"],
        "homepage": v["html_url"],
    });

    let text = format!(
        "{}\n{}\n★ {}  forks {}  lang {}\n{}",
        out["name"].as_str().unwrap_or(""),
        out["description"].as_str().unwrap_or("(no description)"),
        out["stars"].as_u64().unwrap_or(0),
        out["forks"].as_u64().unwrap_or(0),
        out["language"].as_str().unwrap_or("n/a"),
        out["homepage"].as_str().unwrap_or("")
    );
    web::emit(json_out, &out, &text)
}

/// Fetch a repo's README via the GitHub API (base64-encoded content).
pub async fn readme(owner_repo: &str, json_out: bool) -> Result<()> {
    let (o, r) = parts(owner_repo)?;
    let client = http_client()?;

    // Primary: dedicated README endpoint.
    let url = format!("https://api.github.com/repos/{o}/{r}/readme");
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let v: serde_json::Value = resp.json().await?;
            if let Some(b64) = v["content"].as_str() {
                if let Ok(bytes) = base64_decode(b64.trim()) {
                    let md = String::from_utf8_lossy(&bytes).to_string();
                    return web::emit(json_out, &json!({ "repo": format!("{o}/{r}"), "readme": md }), &md);
                }
            }
        }
        _ => {}
    }

    // Fallback: raw README.md on the default branch.
    let meta: serde_json::Value = client.get(format!("https://api.github.com/repos/{o}/{r}")).send().await?.json().await?;
    let branch = meta["default_branch"].as_str().unwrap_or("main");
    let raw = format!("https://raw.githubusercontent.com/{o}/{r}/{branch}/README.md");
    match client.get(&raw).send().await {
        Ok(resp) if resp.status().is_success() => {
            let md = resp.text().await.unwrap_or_default();
            return web::emit(json_out, &json!({ "repo": format!("{o}/{r}"), "readme": md }), &md);
        }
        _ => {}
    }

    Err(anyhow!("no README found for `{owner_repo}`"))
}

/// Minimal base64 decoder (avoids a dependency just for this).
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0usize;
    for c in s.chars() {
        if c == '=' || c.is_whitespace() {
            continue;
        }
        let idx = A.iter().position(|&b| b as char == c).ok_or_else(|| anyhow!("bad base64 char"))? as u32;
        buf = (buf << 6) | idx;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

/// Recent issues on a public repo.
pub async fn issues(owner_repo: &str, n: usize, json_out: bool) -> Result<()> {
    let (o, r) = parts(owner_repo)?;
    let client = http_client()?;
    let url = format!("https://api.github.com/repos/{o}/{r}/issues?state=open&per_page={n}");
    let v: serde_json::Value = client.get(&url).send().await?.json().await?;

    if !v.is_array() {
        return Err(anyhow!("GitHub said: {}", v["message"].as_str().unwrap_or("error")));
    }

    let mut results = Vec::new();
    let mut lines = Vec::new();
    for item in v.as_array().expect("checked is_array above").iter().take(n) {
        if item.get("pull_request").is_some() {
            continue; // skip PRs, keep issues only
        }
        let title = item["title"].as_str().unwrap_or("").to_string();
        let num = item["number"].as_i64().unwrap_or(0);
        let link = item["html_url"].as_str().unwrap_or("").to_string();
        results.push(json!({ "title": title, "number": num, "url": link }));
        lines.push(format!("#{} {title}\n   {link}", num));
    }

    if results.is_empty() {
        return web::emit(json_out, &json!({ "repo": format!("{o}/{r}"), "count": 0 }), "No open issues.");
    }
    web::emit(json_out, &json!({ "repo": format!("{o}/{r}"), "count": results.len(), "issues": results }), &lines.join("\n"))
}
