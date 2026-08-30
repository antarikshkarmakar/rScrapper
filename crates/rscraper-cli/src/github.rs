//! Typed public GitHub REST services using the shared fetch transport.

use crate::context::AppContext;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use reqwest::header::{HeaderValue, ACCEPT};
use rscraper_core::{Error, FetchRequest, RawResponse, Result};
use serde::{Deserialize, Serialize};
use url::Url;

const MAX_ISSUES: usize = 100;
const MAX_ISSUE_PAGES: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct GithubRepo {
    pub name: String,
    pub description: Option<String>,
    pub stars: u64,
    pub forks: u64,
    pub language: Option<String>,
    pub license: Option<String>,
    pub open_issues: u64,
    pub homepage: Url,
    pub default_branch: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GithubReadme {
    pub repo: String,
    pub readme: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GithubIssue {
    pub title: String,
    pub number: u64,
    pub url: Url,
}

#[derive(Debug, Clone, Serialize)]
pub struct GithubIssues {
    pub repo: String,
    pub count: usize,
    pub issues: Vec<GithubIssue>,
}

#[derive(Debug, Clone)]
pub struct ParsedIssuePage {
    pub raw_count: usize,
    pub issues: Vec<GithubIssue>,
}

#[derive(Deserialize)]
struct RepoWire {
    full_name: String,
    description: Option<String>,
    stargazers_count: u64,
    forks_count: u64,
    language: Option<String>,
    license: Option<LicenseWire>,
    open_issues_count: u64,
    html_url: String,
    default_branch: String,
}

#[derive(Deserialize)]
struct LicenseWire {
    spdx_id: Option<String>,
}

#[derive(Deserialize)]
struct ReadmeWire {
    encoding: String,
    content: String,
}

#[derive(Deserialize)]
struct IssueWire {
    number: u64,
    title: String,
    html_url: String,
    pull_request: Option<serde_json::Value>,
}

pub fn validate_owner_repo(owner_repo: &str) -> Result<(&str, &str)> {
    let mut segments = owner_repo.split('/');
    let owner = segments.next().unwrap_or_default();
    let repo = segments.next().unwrap_or_default();
    if owner.is_empty()
        || repo.is_empty()
        || segments.next().is_some()
        || !owner.chars().all(valid_repo_component)
        || !repo.chars().all(valid_repo_component)
    {
        return Err(Error::InvalidInput(
            "GitHub repository must be exactly `owner/repo`".into(),
        ));
    }
    Ok((owner, repo))
}

pub fn parse_repo_response(bytes: &[u8]) -> Result<GithubRepo> {
    let wire: RepoWire =
        serde_json::from_slice(bytes).map_err(|error| parse_error(error, "repo"))?;
    let homepage = Url::parse(&wire.html_url)
        .ok()
        .filter(github_url)
        .ok_or_else(|| Error::Parse {
            kind: "github repo",
            message: "repository URL is invalid".into(),
        })?;
    Ok(GithubRepo {
        name: wire.full_name,
        description: wire.description,
        stars: wire.stargazers_count,
        forks: wire.forks_count,
        language: wire.language,
        license: wire.license.and_then(|license| license.spdx_id),
        open_issues: wire.open_issues_count,
        homepage,
        default_branch: wire.default_branch,
    })
}

pub fn parse_readme_response(bytes: &[u8], owner_repo: &str) -> Result<GithubReadme> {
    let wire: ReadmeWire =
        serde_json::from_slice(bytes).map_err(|error| parse_error(error, "readme"))?;
    if !wire.encoding.eq_ignore_ascii_case("base64") {
        return Err(Error::Parse {
            kind: "github readme",
            message: "unsupported README encoding".into(),
        });
    }
    let compact = wire
        .content
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    let decoded = STANDARD.decode(compact).map_err(|_| Error::Parse {
        kind: "github readme",
        message: "README base64 is invalid".into(),
    })?;
    let readme = String::from_utf8(decoded).map_err(|_| Error::Parse {
        kind: "github readme",
        message: "README is not UTF-8".into(),
    })?;
    Ok(GithubReadme {
        repo: owner_repo.to_string(),
        readme,
    })
}

pub fn parse_issues_response(bytes: &[u8]) -> Result<ParsedIssuePage> {
    let wire: Vec<IssueWire> =
        serde_json::from_slice(bytes).map_err(|error| parse_error(error, "issues"))?;
    let raw_count = wire.len();
    let issues = wire
        .into_iter()
        .filter(|issue| issue.pull_request.is_none())
        .map(|issue| {
            let url = Url::parse(&issue.html_url)
                .ok()
                .filter(github_url)
                .ok_or_else(|| Error::Parse {
                    kind: "github issues",
                    message: "issue URL is invalid".into(),
                })?;
            Ok(GithubIssue {
                title: issue.title,
                number: issue.number,
                url,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ParsedIssuePage { raw_count, issues })
}

pub async fn repo(context: &AppContext, owner_repo: &str) -> Result<GithubRepo> {
    repo_with_api_base(context, owner_repo, &github_api_base()).await
}

pub async fn repo_with_api_base(
    context: &AppContext,
    owner_repo: &str,
    api_base: &Url,
) -> Result<GithubRepo> {
    let (owner, repo) = validate_owner_repo(owner_repo)?;
    let url = endpoint(api_base, &format!("repos/{owner}/{repo}"))?;
    let response = fetch_json(context, url).await?;
    ensure_github_success(&response)?;
    parse_repo_response(&response.bytes)
}

pub async fn readme(context: &AppContext, owner_repo: &str) -> Result<GithubReadme> {
    readme_with_api_base(context, owner_repo, &github_api_base()).await
}

pub async fn readme_with_api_base(
    context: &AppContext,
    owner_repo: &str,
    api_base: &Url,
) -> Result<GithubReadme> {
    let (owner, repo) = validate_owner_repo(owner_repo)?;
    let url = endpoint(api_base, &format!("repos/{owner}/{repo}/readme"))?;
    let response = fetch_json(context, url).await?;
    ensure_github_success(&response)?;
    parse_readme_response(&response.bytes, owner_repo)
}

pub async fn issues(context: &AppContext, owner_repo: &str, count: usize) -> Result<GithubIssues> {
    issues_with_api_base(context, owner_repo, count, &github_api_base()).await
}

pub async fn issues_with_api_base(
    context: &AppContext,
    owner_repo: &str,
    count: usize,
    api_base: &Url,
) -> Result<GithubIssues> {
    let (owner, repo) = validate_owner_repo(owner_repo)?;
    if !(1..=MAX_ISSUES).contains(&count) {
        return Err(Error::InvalidInput(format!(
            "GitHub issue count must be between 1 and {MAX_ISSUES}"
        )));
    }
    let per_page = count.min(100);
    let mut collected = Vec::new();
    let mut complete = false;
    for page in 1..=MAX_ISSUE_PAGES {
        let mut url = endpoint(api_base, &format!("repos/{owner}/{repo}/issues"))?;
        url.query_pairs_mut()
            .append_pair("state", "open")
            .append_pair("per_page", &per_page.to_string())
            .append_pair("page", &page.to_string());
        let response = fetch_json(context, url).await?;
        ensure_github_success(&response)?;
        let parsed = parse_issues_response(&response.bytes)?;
        let raw_count = parsed.raw_count;
        collected.extend(parsed.issues.into_iter().take(count - collected.len()));
        if collected.len() == count || raw_count < per_page {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(Error::Parse {
            kind: "github issues",
            message: format!(
                "pagination exceeded the {MAX_ISSUE_PAGES}-page safety limit before reaching the API end"
            ),
        });
    }

    Ok(GithubIssues {
        repo: owner_repo.to_string(),
        count: collected.len(),
        issues: collected,
    })
}

async fn fetch_json(context: &AppContext, url: Url) -> Result<RawResponse> {
    let mut request = FetchRequest::request(url.as_str())?;
    request.headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json, application/json"),
    );
    context.fetch.fetch_raw_request(request).await
}

fn ensure_github_success(response: &RawResponse) -> Result<()> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }
    if response.status == 429
        || (response.status == 403
            && (response.rate_limit.remaining == Some(0)
                || github_message_is_rate_limit(&response.bytes)))
    {
        return Err(Error::RateLimited {
            retry_after_secs: response.rate_limit.retry_after_secs,
        });
    }
    Err(Error::HttpStatus {
        status: response.status,
        url: response.url.clone(),
    })
}

fn github_message_is_rate_limit(bytes: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Message {
        message: Option<String>,
    }
    serde_json::from_slice::<Message>(bytes)
        .ok()
        .and_then(|message| message.message)
        .is_some_and(|message| message.to_ascii_lowercase().contains("rate limit"))
}

fn endpoint(base: &Url, relative: &str) -> Result<Url> {
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(Error::InvalidInput("GitHub API base URL is invalid".into()));
    }
    base.join(relative)
        .map_err(|_| Error::InvalidInput("GitHub API endpoint is invalid".into()))
}

fn github_api_base() -> Url {
    Url::parse("https://api.github.com/").expect("static GitHub API URL is valid")
}

fn valid_repo_component(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
}

fn github_url(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host == "github.com" || host.ends_with(".github.com")
    })
}

fn parse_error(error: serde_json::Error, resource: &'static str) -> Error {
    Error::Parse {
        kind: match resource {
            "repo" => "github repo",
            "readme" => "github readme",
            _ => "github issues",
        },
        message: format!(
            "invalid JSON at line {} column {}",
            error.line(),
            error.column()
        ),
    }
}
