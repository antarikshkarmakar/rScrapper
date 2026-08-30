//! Sole owner of CLI stdout/stderr presentation.

use crate::{doctor, github, social, web};
use anyhow::Result as AnyResult;
use rscraper_core::Error;
use serde::Serialize;
use serde_json::json;
use std::io::{self, Write};

pub fn emit_value<T: Serialize>(json_output: bool, value: &T, human: &str) -> AnyResult<()> {
    if json_output {
        writeln!(io::stdout(), "{}", serde_json::to_string(value)?)?;
    } else {
        writeln!(io::stdout(), "{human}")?;
    }
    Ok(())
}

pub fn emit_json_value(json_output: bool, value: &serde_json::Value, human: &str) -> AnyResult<()> {
    emit_value(json_output, value, human)
}

pub fn emit_error(json_output: bool, error: &anyhow::Error) -> AnyResult<()> {
    let (code, message) = error
        .downcast_ref::<Error>()
        .map(core_error)
        .unwrap_or(("operation_failed", "operation failed".into()));
    if json_output {
        writeln!(
            io::stdout(),
            "{}",
            serde_json::to_string(&json!({
                "error": {
                    "code": code,
                    "message": message,
                }
            }))?
        )?;
    } else {
        writeln!(io::stderr(), "error: {message}")?;
    }
    Ok(())
}

pub fn emit_cli_parse_error(json_output: bool, error: &clap::Error) -> AnyResult<()> {
    if json_output && error.exit_code() != 0 {
        let message = match error.kind() {
            clap::error::ErrorKind::InvalidValue
            | clap::error::ErrorKind::ValueValidation
            | clap::error::ErrorKind::NoEquals
            | clap::error::ErrorKind::TooManyValues
            | clap::error::ErrorKind::TooFewValues
            | clap::error::ErrorKind::WrongNumberOfValues => "invalid command-line value",
            clap::error::ErrorKind::MissingRequiredArgument
            | clap::error::ErrorKind::MissingSubcommand => {
                "required command-line argument is missing"
            }
            clap::error::ErrorKind::UnknownArgument | clap::error::ErrorKind::InvalidSubcommand => {
                "unknown command or argument"
            }
            _ => "invalid command line",
        };
        writeln!(
            io::stdout(),
            "{}",
            serde_json::to_string(&json!({
                "error": {
                    "code": "cli_parse",
                    "message": message,
                }
            }))?
        )?;
    } else {
        error.print()?;
    }
    Ok(())
}

pub fn render_read(response: &web::ReadResponse) -> String {
    response.markdown.clone()
}

pub fn render_search(response: &web::SearchResponse) -> String {
    if response.results.is_empty() {
        return "No results.".into();
    }
    let mut lines = Vec::new();
    if let Some(warning) = &response.fallback_warning {
        lines.push(format!("warning: {warning}"));
    }
    for (index, hit) in response.results.iter().enumerate() {
        lines.push(format!("{}. {}\n   {}", index + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            lines.push(format!("   {}", hit.snippet));
        }
        if let Some(markdown) = &hit.markdown {
            lines.push(markdown.clone());
        }
        if let Some(error) = &hit.scrape_error {
            lines.push(format!("   scrape error: {error}"));
        }
    }
    lines.join("\n")
}

pub fn render_repo(repo: &github::GithubRepo) -> String {
    format!(
        "{}\n{}\n★ {}  forks {}  lang {}\n{}",
        repo.name,
        repo.description.as_deref().unwrap_or("(no description)"),
        repo.stars,
        repo.forks,
        repo.language.as_deref().unwrap_or("n/a"),
        repo.homepage
    )
}

pub fn render_issues(issues: &github::GithubIssues) -> String {
    if issues.issues.is_empty() {
        return "No open issues.".into();
    }
    issues
        .issues
        .iter()
        .map(|issue| format!("#{} {}\n   {}", issue.number, issue.title, issue.url))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_reddit(response: &social::RedditResponse) -> String {
    response
        .results
        .iter()
        .map(|post| format!("{} (▲{})\n   {}", post.title, post.score, post.url))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_bilibili(response: &social::BilibiliResponse) -> String {
    response
        .results
        .iter()
        .map(|video| format!("{} — {}", video.title, video.url))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_setup(response: &social::SetupResponse) -> String {
    let mut text = format!(
        "Setup for {} (login required: {})\n\n{}",
        response.platform,
        response.needs_login,
        response.steps.join("\n")
    );
    if let Some(path) = &response.cookie_path {
        text.push_str(&format!("\n\nPrivate cookie file: {}", path.display()));
    }
    text
}

pub fn render_doctor(report: &doctor::DoctorReport) -> String {
    let mut text = String::from("rScrapper doctor\n");
    for check in &report.checks {
        let icon = match check.status {
            doctor::Status::Ok => "✓",
            doctor::Status::Warn => "!",
            doctor::Status::Fail => "✗",
        };
        text.push_str(&format!("  {icon} {}\n     {}\n", check.name, check.detail));
        if let Some(fix) = &check.fix {
            text.push_str(&format!("     fix: {fix}\n"));
        }
    }
    text.push_str(if report.all_ok {
        "\nAll core checks passed; warnings are optional capabilities."
    } else {
        "\nOne or more core checks failed."
    });
    text
}

fn core_error(error: &Error) -> (&'static str, String) {
    match error {
        Error::InvalidInput(message) => ("invalid_input", message.clone()),
        Error::Policy(_) => (
            "policy_rejected",
            "network or filesystem policy rejected the operation".into(),
        ),
        Error::Dns(_) => ("dns_failed", "DNS resolution failed".into()),
        Error::Timeout { operation } => ("timeout", format!("{operation} operation timed out")),
        Error::BodyLimit { limit } => (
            "body_limit",
            format!("response exceeded the {limit}-byte limit"),
        ),
        Error::HttpStatus { status, .. } => {
            ("http_status", format!("upstream returned HTTP {status}"))
        }
        Error::Browser(_) => ("browser_failed", "browser rendering failed".into()),
        Error::Parse { kind, .. } => (
            "parse_failed",
            format!("{kind} response could not be parsed"),
        ),
        Error::Authentication(message) => ("authentication", message.clone()),
        Error::RateLimited { retry_after_secs } => (
            "rate_limited",
            retry_after_secs.map_or_else(
                || "upstream rate limit reached".into(),
                |seconds| format!("upstream rate limit reached; retry after {seconds} seconds"),
            ),
        ),
        Error::RobotsDenied(_) => ("robots_denied", "robots policy denied the URL".into()),
        Error::Cancelled => ("cancelled", "operation cancelled".into()),
        Error::UpstreamLayout { service } => (
            "upstream_layout",
            format!("{service} response layout changed"),
        ),
        Error::Io(_) => ("io_failed", "filesystem operation failed".into()),
        Error::Http(_) => ("transport_failed", "HTTP transport failed".into()),
    }
}
