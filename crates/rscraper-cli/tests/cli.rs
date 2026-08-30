use assert_cmd::Command;
use rscraper_cli::{github, output, web};
use serde_json::Value;
use tempfile::TempDir;

#[test]
fn no_arguments_prints_the_cheatsheet_successfully() {
    let output = rscraper().output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("get <url-or-query>"));
    assert!(stdout.contains("doctor"));
    assert!(output.stderr.is_empty());
}

#[test]
fn every_documented_subcommand_has_help_without_network_access() {
    for arguments in [
        vec!["get", "--help"],
        vec!["read", "--help"],
        vec!["search", "--help"],
        vec!["youtube", "--help"],
        vec!["youtube", "subs", "--help"],
        vec!["youtube", "search", "--help"],
        vec!["github", "--help"],
        vec!["github", "repo", "--help"],
        vec!["github", "readme", "--help"],
        vec!["github", "issues", "--help"],
        vec!["rss", "--help"],
        vec!["social", "--help"],
        vec!["social", "twitter", "--help"],
        vec!["social", "reddit", "--help"],
        vec!["social", "bilibili", "--help"],
        vec!["social", "xiaohongshu", "--help"],
        vec!["social", "linkedin", "--help"],
        vec!["setup", "--help"],
        vec!["doctor", "--help"],
    ] {
        let output = rscraper().args(&arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    }
}

#[test]
fn json_errors_have_stable_codes_and_messages_on_stdout() {
    for (arguments, expected_fragment) in [
        (vec!["read", "not-a-url"], "invalid URL"),
        (vec!["github", "repo", "owner/repo/extra"], "owner/repo"),
        (vec!["search", "rust", "--n", "21"], "between 1 and 20"),
    ] {
        let output = rscraper().arg("--json").args(&arguments).output().unwrap();
        assert!(!output.status.success(), "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "invalid_input");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected_fragment),
            "{arguments:?}: {value}"
        );
    }
}

#[test]
fn parser_level_json_errors_are_one_stable_value_on_stdout() {
    for (arguments, expected_message) in [
        (
            vec!["--json", "search", "rust", "--n", "not-a-number"],
            "invalid command-line value",
        ),
        (
            vec!["--json", "read"],
            "required command-line argument is missing",
        ),
        (
            vec!["--json", "not-a-command"],
            "unknown command or argument",
        ),
    ] {
        let output = rscraper().args(&arguments).output().unwrap();
        assert!(!output.status.success(), "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "cli_parse");
        assert_eq!(value["error"]["message"], expected_message);
    }
}

#[test]
fn json_pre_detection_stops_at_the_option_terminator_but_accepts_global_positions() {
    let positional = rscraper()
        .args(["read", "--", "--json", "extra"])
        .output()
        .unwrap();
    assert!(!positional.status.success());
    assert!(positional.stdout.is_empty());
    assert!(String::from_utf8_lossy(&positional.stderr).contains("error:"));

    for arguments in [["--json", "read"], ["read", "--json"]] {
        let output = rscraper().args(arguments).output().unwrap();
        assert!(!output.status.success(), "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "cli_parse");
        assert_eq!(
            value["error"]["message"],
            "required command-line argument is missing"
        );
    }
}

#[test]
fn unknown_setup_platform_reports_the_actual_name_before_filesystem_mutation() {
    let directory = TempDir::new().unwrap();
    let state = directory.path().join("state");
    let output = rscraper()
        .env("RSCRAPER_HOME", &state)
        .args(["--json", "setup", "mystery-platform"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "invalid_input");
    assert!(value["error"]["message"]
        .as_str()
        .unwrap()
        .contains("mystery-platform"));
    assert!(!state.exists());
}

#[test]
fn fixture_free_setup_json_keeps_the_documented_output_shape() {
    let directory = TempDir::new().unwrap();
    let output = rscraper()
        .env("RSCRAPER_HOME", directory.path().join("state"))
        .args(["--json", "setup", "bilibili"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["platform"], "bilibili");
    assert_eq!(value["needs_login"], "no");
    assert!(value["steps"]
        .as_array()
        .is_some_and(|steps| !steps.is_empty()));
}

#[test]
fn sanitized_fixtures_produce_stable_typed_cli_output_shapes() {
    let repo = github::parse_repo_response(include_bytes!("fixtures/github-repo.json")).unwrap();
    let repo_json = serde_json::to_value(&repo).unwrap();
    assert_eq!(repo_json["name"], "acme/widget");
    assert!(repo_json["stars"].is_u64());
    assert!(repo_json["homepage"].is_string());
    assert!(output::render_repo(&repo).contains("acme/widget"));

    let hits = web::parse_duckduckgo_results(include_str!("fixtures/ddg-results.html"), 2).unwrap();
    let search = web::SearchResponse {
        query: "fixture".into(),
        count: hits.len(),
        results: hits,
        provider: "duckduckgo",
        fallback_warning: None,
    };
    let search_json = serde_json::to_value(&search).unwrap();
    assert_eq!(search_json["provider"], "duckduckgo");
    assert_eq!(search_json["results"].as_array().unwrap().len(), 2);
    assert!(output::render_search(&search).contains("Alpha & Beta"));
}

fn rscraper() -> Command {
    Command::cargo_bin("rscraper").unwrap()
}
