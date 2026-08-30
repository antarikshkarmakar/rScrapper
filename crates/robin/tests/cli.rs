mod support;

use async_trait::async_trait;
use robin::cli::{run_with_io, CliConfig, InvestigationRunner};
use robin::{Error, ErrorCode, Report, Result};
use std::io::Cursor;
use support::CallLog;

#[derive(Clone, Default)]
struct RecordingRunner {
    calls: CallLog,
}

#[async_trait]
impl InvestigationRunner for RecordingRunner {
    async fn investigate(&self, config: &CliConfig) -> Result<Report> {
        self.calls
            .push(format!("{}:{}", config.query, config.provider.name()))
            .await;
        Err(Error::tor_unavailable())
    }
}

#[tokio::test]
async fn positional_and_long_query_forms_reach_the_same_validated_config() {
    for args in [
        vec![
            "robin",
            "query text",
            "--provider",
            "ollama",
            "--model",
            "llama3",
        ],
        vec![
            "robin",
            "--query",
            "query text",
            "--provider",
            "ollama",
            "--model",
            "llama3",
        ],
        vec![
            "robin",
            " query   text ",
            "--query",
            "query text",
            "--provider",
            "ollama",
            "--model",
            "llama3",
        ],
    ] {
        let runner = RecordingRunner::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = run_with_io(
            args,
            Cursor::new(Vec::<u8>::new()),
            &mut stdout,
            &mut stderr,
            &runner,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::TorUnavailable);
        assert_eq!(runner.calls.snapshot().await, vec!["query text:ollama"]);
        assert!(stderr.is_empty());
    }
}

#[tokio::test]
async fn different_dual_queries_fail_before_the_runner() {
    let runner = RecordingRunner::default();
    let error = run_with_io(
        [
            "robin",
            "one",
            "--query",
            "two",
            "--provider",
            "ollama",
            "--model",
            "llama3",
        ],
        Cursor::new(Vec::<u8>::new()),
        Vec::new(),
        Vec::new(),
        &runner,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert!(runner.calls.snapshot().await.is_empty());
}

#[tokio::test]
async fn dry_run_validates_without_tor_provider_runner_or_report_claims() {
    let runner = RecordingRunner::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run_with_io(
        [
            "robin",
            "query",
            "--provider",
            "ollama",
            "--model",
            "llama3",
            "--dry-run",
        ],
        Cursor::new(Vec::<u8>::new()),
        &mut stdout,
        &mut stderr,
        &runner,
    )
    .await
    .unwrap();
    let stdout = String::from_utf8(stdout).unwrap();
    assert!(stdout.contains("validated"));
    assert!(stdout.contains("No Tor or provider connection was attempted"));
    assert!(!stdout.contains("Tor checked"));
    assert!(!stdout.contains("report generated"));
    assert!(stderr.is_empty());
    assert!(runner.calls.snapshot().await.is_empty());
}

#[tokio::test]
async fn dry_run_rejects_forbidden_ipv4_mapped_proxy_literals_before_side_effects() {
    for proxy in [
        "socks5h://[::ffff:0.0.0.0]:9050/",
        "socks5h://[::ffff:224.0.0.1]:9050/",
        "socks5h://[::ffff:255.255.255.255]:9050/",
    ] {
        let runner = RecordingRunner::default();
        let mut stdout = Vec::new();
        let error = run_with_io(
            [
                "robin",
                "query",
                "--provider",
                "ollama",
                "--model",
                "llama3",
                "--tor",
                proxy,
                "--dry-run",
            ],
            Cursor::new(Vec::<u8>::new()),
            &mut stdout,
            Vec::new(),
            &runner,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidInput, "accepted {proxy}");
        assert!(stdout.is_empty());
        assert!(runner.calls.snapshot().await.is_empty());
    }
}

#[tokio::test]
async fn interactive_mode_prompts_only_for_missing_values() {
    let runner = RecordingRunner::default();
    let input = b"interactive query\nollama\nllama3\nsocks5h://127.0.0.1:9050/\nreports\n";
    let mut stdout = Vec::new();
    let error = run_with_io(
        ["robin", "--interactive"],
        Cursor::new(input),
        &mut stdout,
        Vec::new(),
        &runner,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::TorUnavailable);
    let output = String::from_utf8(stdout).unwrap();
    for prompt in ["Query", "Provider", "Model", "Tor proxy", "Save directory"] {
        assert!(output.contains(prompt), "missing {prompt}: {output}");
    }
    assert!(!output.contains("provider-secret"));
}

#[tokio::test]
async fn empty_or_eof_interactive_input_is_a_stable_error() {
    for input in [b"".as_slice(), b"\n".as_slice()] {
        let runner = RecordingRunner::default();
        let error = run_with_io(
            ["robin", "--interactive"],
            Cursor::new(input),
            Vec::new(),
            Vec::new(),
            &runner,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidInput);
        assert!(runner.calls.snapshot().await.is_empty());
    }
}

#[tokio::test]
async fn invalid_model_proxy_and_save_path_fail_before_any_side_effect() {
    let oversized_path = "a".repeat(4_097);
    let cases = vec![
        vec!["robin", "q", "--provider", "ollama", "--model", "bad\u{7f}"],
        vec![
            "robin",
            "q",
            "--provider",
            "ollama",
            "--model",
            "llama3",
            "--tor",
            "socks5://127.0.0.1:9050/",
        ],
        vec![
            "robin",
            "q",
            "--provider",
            "ollama",
            "--model",
            "llama3",
            "--save",
            "",
        ],
        vec![
            "robin",
            "q",
            "--provider",
            "ollama",
            "--model",
            "llama3",
            "--save",
            "bad\npath",
        ],
        vec![
            "robin",
            "q",
            "--provider",
            "ollama",
            "--model",
            "llama3",
            "--save",
            oversized_path.as_str(),
        ],
    ];
    for args in cases {
        let runner = RecordingRunner::default();
        let error = run_with_io(
            args,
            Cursor::new(Vec::<u8>::new()),
            Vec::new(),
            Vec::new(),
            &runner,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidInput);
        assert!(runner.calls.snapshot().await.is_empty());
    }
}

#[test]
fn help_documents_query_forms_tor_fail_closed_keys_bounds_and_dry_run() {
    use clap::CommandFactory;
    let mut command = robin::cli::Args::command();
    let help = command.render_long_help().to_string();
    for text in [
        "[QUERY]",
        "--query",
        "socks5h://127.0.0.1:9050/",
        "fail closed",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GEMINI_API_KEY",
        "OLLAMA_HOST",
        "--dry-run",
        "five",
        "report",
    ] {
        assert!(help.contains(text), "missing {text}: {help}");
    }
    assert!(!help.to_ascii_lowercase().contains("captcha bypass"));
}

#[tokio::test]
async fn explicit_invalid_values_are_rejected_before_interactive_prompts() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("not-a-directory");
    std::fs::write(&file, "preserve").unwrap();
    let cases = [
        vec![
            "robin".into(),
            "--interactive".into(),
            "--tor".into(),
            "socks5://127.0.0.1:9050/".into(),
        ],
        vec![
            "robin".into(),
            "--interactive".into(),
            "--model".into(),
            "bad model".into(),
        ],
        vec![
            "robin".into(),
            "--interactive".into(),
            "--save".into(),
            file.to_string_lossy().into_owned(),
        ],
    ];
    for args in cases {
        let runner = RecordingRunner::default();
        let mut stdout = Vec::new();
        let error = run_with_io(
            args,
            Cursor::new(Vec::<u8>::new()),
            &mut stdout,
            Vec::new(),
            &runner,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error.code(),
            ErrorCode::InvalidInput | ErrorCode::Policy
        ));
        assert!(
            stdout.is_empty(),
            "prompted before validation: {}",
            String::from_utf8_lossy(&stdout)
        );
        assert!(runner.calls.snapshot().await.is_empty());
    }
}

#[tokio::test]
async fn interactive_mode_does_not_prompt_for_explicit_values() {
    let runner = RecordingRunner::default();
    let mut stdout = Vec::new();
    run_with_io(
        [
            "robin",
            "query",
            "--interactive",
            "--provider",
            "ollama",
            "--model",
            "llama3",
            "--tor",
            "socks5h://127.0.0.1:9050/",
            "--save",
            "reports",
            "--dry-run",
        ],
        Cursor::new(Vec::<u8>::new()),
        &mut stdout,
        Vec::new(),
        &runner,
    )
    .await
    .unwrap();
    let output = String::from_utf8(stdout).unwrap();
    for prompt in [
        "Query:",
        "Provider:",
        "Model:",
        "Tor proxy:",
        "Save directory:",
    ] {
        assert!(
            !output.contains(prompt),
            "unexpected prompt {prompt}: {output}"
        );
    }
}

#[test]
fn binary_dry_run_keeps_stdout_human_and_stderr_empty_without_network() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_robin"))
        .args([
            "query",
            "--provider",
            "ollama",
            "--model",
            "llama3",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Configuration validated"));
    assert!(output.stderr.is_empty());
}
