use std::collections::BTreeMap;
use std::process::Command;

const README: &str = include_str!("../../../README.md");
const MIGRATION: &str = include_str!("../../../MIGRATION.md");
const OFFLINE_START: &str = "<!-- rscraper-readme-offline:start -->";
const OFFLINE_END: &str = "<!-- rscraper-readme-offline:end -->";
const OLD_FETCH_INTRO: &str = "The 0.1 compatibility call was:";

#[test]
fn readme_contract_rejects_prohibited_release_claims() {
    let lower = README.to_ascii_lowercase();
    for prohibited in [
        "free internet access",
        "one static binary",
        "self-contained static binary",
        "nothing phones home",
        "only ever sent",
        "invalid tls",
        "invalid-tls",
        "accept invalid certificates",
        "disable tls verification",
        "bypass captcha",
        "bypass captchas",
        "captcha bypass",
        "works on every site",
        "access every site",
        "cookies never leave",
    ] {
        assert!(
            !lower.contains(prohibited),
            "README contains prohibited claim: {prohibited}"
        );
    }
}

#[test]
fn readme_and_migration_describe_browser_and_auto_fallback_truthfully() {
    for (name, document) in [("README", README), ("migration guide", MIGRATION)] {
        let normalized = document
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        assert!(
            normalized.contains("explicit browser mode requires a configured renderer"),
            "{name} must state the explicit Browser requirement"
        );
        assert!(
            normalized.contains("auto mode does not require a browser"),
            "{name} must state that Auto works without a browser"
        );
        assert!(
            normalized.contains("fetches http first"),
            "{name} must state Auto's HTTP-first behavior"
        );
        assert!(
            normalized.contains(
                "returns the original http page if no renderer is available or rendering fails"
            ),
            "{name} must state Auto's exact fallback behavior"
        );
        assert!(
            !normalized.contains("browser and auto modes require")
                && !normalized.contains("browser and auto modes need"),
            "{name} must not make Chromium mandatory for Auto"
        );
    }
}

#[test]
fn readme_migration_old_fetch_example_compiles_against_frozen_0_1_surface() {
    let snippet = MIGRATION
        .split_once(OLD_FETCH_INTRO)
        .and_then(|(_, remainder)| remainder.split_once("```rust").map(|(_, code)| code))
        .and_then(|code| code.split_once("```").map(|(code, _)| code.trim()))
        .expect("migration guide must contain the old 0.1 Rust example");

    let mut replaced_runtime_wrapper = false;
    let fixture_example = snippet
        .lines()
        .filter_map(|line| {
            if line.trim() == "#[tokio::main]" {
                return None;
            }
            if line.trim() == "async fn main() -> anyhow::Result<()> {" {
                replaced_runtime_wrapper = true;
                return Some(
                    "async fn documented_old_fetch() -> Result<(), rscraper_core::FrozenError> {"
                        .to_owned(),
                );
            }
            Some(line.to_owned())
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        replaced_runtime_wrapper,
        "old example must retain one recognizable async runtime wrapper"
    );

    let source = format!(
        r#"#![allow(dead_code)]

mod rscraper_core {{
    #[derive(Debug)]
    pub struct FrozenError;

    pub mod client {{
        #[derive(Clone, Copy)]
        pub enum FetchMode {{
            Request,
            Browser,
            Auto,
        }}
    }}

    pub mod fetch {{
        use super::FrozenError;

        #[derive(Clone, Copy, Default)]
        pub enum FetchMode {{
            Request,
            Js,
            Stealth,
            #[default]
            Auto,
        }}

        #[derive(Default)]
        pub struct FetchOptions {{
            mode: FetchMode,
        }}

        impl FetchOptions {{
            pub fn new() -> Self {{
                Self::default()
            }}

            pub fn mode(mut self, mode: FetchMode) -> Self {{
                self.mode = mode;
                self
            }}
        }}

        pub struct Page {{
            pub html: String,
        }}

        pub async fn fetch(
            _url: &str,
            _options: &FetchOptions,
        ) -> Result<Page, FrozenError> {{
            Ok(Page {{ html: String::new() }})
        }}
    }}

    pub use client::FetchMode;
    pub use fetch::{{fetch, FetchOptions}};
}}

{fixture_example}
"#
    );
    let fixture = tempfile::tempdir().expect("migration compile fixture directory");
    let source_path = fixture.path().join("migration_0_1.rs");
    std::fs::write(&source_path, source).expect("write migration compile fixture");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .args(["--edition=2021", "--crate-type=lib"])
        .arg(&source_path)
        .arg("--out-dir")
        .arg(fixture.path())
        .output()
        .expect("run rustc for migration compile fixture");
    assert!(
        output.status.success(),
        "old migration example does not compile against the frozen 0.1 surface:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn readme_contract_executes_every_marked_offline_command() {
    let block = README
        .split_once(OFFLINE_START)
        .and_then(|(_, remainder)| remainder.split_once(OFFLINE_END).map(|(block, _)| block))
        .expect("README must contain one offline command marker block");
    assert_eq!(README.matches(OFFLINE_START).count(), 1);
    assert_eq!(README.matches(OFFLINE_END).count(), 1);

    let commands: Vec<&str> = block
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("```") && !line.starts_with('#'))
        .collect();
    assert!(
        !commands.is_empty(),
        "README offline marker block must execute at least one command"
    );

    for marked in commands {
        let (expectation, command) = marked
            .split_once(" | ")
            .expect("marked commands use `success | ...` or `failure | ...`");
        assert!(matches!(expectation, "success" | "failure"));
        let invocation = parse_safe_offline_command(command);
        let mut process = Command::new(env!("CARGO"));
        process
            .args(&invocation.args)
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .env_remove("RSCRAPER_LIVE_TESTS")
            .env_remove("RSCRAPER_BIND")
            .env_remove("PORT")
            .env_remove("RSCRAPER_API_TOKEN")
            .env_remove("RSCRAPER_API_MAX_CONCURRENT_OPERATIONS")
            .env_remove("RSCRAPER_HOME")
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GEMINI_API_KEY")
            .env_remove("OLLAMA_HOST");
        for (name, value) in &invocation.environment {
            process.env(name, value);
        }
        let output = process.output().expect("marked README command must run");
        let succeeded = output.status.success();
        assert_eq!(
            succeeded,
            expectation == "success",
            "README command had unexpected status: {command}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

struct SafeInvocation {
    environment: BTreeMap<String, String>,
    args: Vec<String>,
}

fn parse_safe_offline_command(command: &str) -> SafeInvocation {
    assert!(
        !command
            .chars()
            .any(|character| matches!(character, ';' | '&' | '|' | '>' | '<' | '`' | '$')),
        "shell syntax is forbidden in executable README commands"
    );
    let mut tokens = command.split_whitespace().peekable();
    let mut environment = BTreeMap::new();
    if tokens.peek() == Some(&"env") {
        tokens.next();
        while let Some(token) = tokens.peek().copied() {
            let Some((name, value)) = token.split_once('=') else {
                break;
            };
            assert!(
                matches!(name, "RSCRAPER_BIND" | "RSCRAPER_API_TOKEN"),
                "unknown README environment variable: {name}"
            );
            assert!(
                !value.is_empty() && value.chars().all(|character| character.is_ascii_graphic())
            );
            environment.insert(name.to_owned(), value.to_owned());
            tokens.next();
        }
    }
    assert_eq!(tokens.next(), Some("cargo"));
    let args: Vec<String> = tokens.map(str::to_owned).collect();
    assert_safe_cargo_run(&args);
    SafeInvocation { environment, args }
}

fn assert_safe_cargo_run(args: &[String]) {
    assert!(args.len() >= 9, "README command is incomplete");
    let package = args[5].as_str();
    let expected_binary = match package {
        "rscraper-cli" => "rscraper",
        "rscraper-api" => "rscraper-api",
        "robin" => "robin",
        _ => panic!("unknown README package: {package}"),
    };
    assert_eq!(
        args[..7].iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "run",
            "--locked",
            "--offline",
            "--quiet",
            "-p",
            package,
            "--bin"
        ]
    );
    assert_eq!(args[7], expected_binary);
    assert_eq!(args[8], "--");

    let binary = args[7].as_str();
    let command_args: Vec<&str> = args[9..].iter().map(String::as_str).collect();
    match (package, binary, command_args.as_slice()) {
        ("rscraper-cli", "rscraper", ["--help" | "--version"])
        | (
            "rscraper-cli",
            "rscraper",
            ["get" | "read" | "search" | "youtube" | "github" | "rss" | "social" | "setup"
            | "doctor", "--help"],
        )
        | ("robin", "robin", ["--help" | "--version"])
        | (
            "robin",
            "robin",
            ["fixture", "--provider", "ollama", "--model", "llama3", "--dry-run"],
        )
        | ("rscraper-api", "rscraper-api", []) => {}
        _ => panic!("unknown or live README command: {}", args.join(" ")),
    }
}
