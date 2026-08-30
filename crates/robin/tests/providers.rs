mod support;

use robin::providers::{ClaudeProvider, GeminiProvider, OllamaProvider, OpenAiProvider};
use robin::{ChatProvider, ErrorCode};
use serde_json::{json, Value};
use support::{hanging_server, json_response, one_shot_server};

#[tokio::test]
async fn provider_adapters_send_exact_typed_requests_and_extract_text() {
    let cases = [
        (
            "openai",
            json_response(
                200,
                r#"{"choices":[{"message":{"content":"openai answer"}}]}"#,
                &[],
            ),
            "/v1/chat/completions",
            "openai answer",
        ),
        (
            "claude",
            json_response(
                200,
                r#"{"content":[{"type":"text","text":"claude answer"}]}"#,
                &[],
            ),
            "/v1/messages",
            "claude answer",
        ),
        (
            "gemini",
            json_response(
                200,
                r#"{"candidates":[{"content":{"parts":[{"text":"gemini answer"}]}}]}"#,
                &[],
            ),
            "/v1beta/models/gemini-test:generateContent?key=gemini-secret",
            "gemini answer",
        ),
        (
            "ollama",
            json_response(200, r#"{"message":{"content":"ollama answer"}}"#, &[]),
            "/api/chat",
            "ollama answer",
        ),
    ];

    for (kind, response, expected_target, expected_text) in cases {
        let (base, request) = one_shot_server(response).await;
        let provider: Box<dyn ChatProvider> = match kind {
            "openai" => Box::new(
                OpenAiProvider::new(
                    "gpt-test",
                    base.join("v1/chat/completions").unwrap(),
                    "openai-secret",
                )
                .unwrap(),
            ),
            "claude" => Box::new(
                ClaudeProvider::new(
                    "claude-test",
                    base.join("v1/messages").unwrap(),
                    "claude-secret",
                )
                .unwrap(),
            ),
            "gemini" => Box::new(
                GeminiProvider::new(
                    "gemini-test",
                    base.join("v1beta/").unwrap(),
                    "gemini-secret",
                )
                .unwrap(),
            ),
            "ollama" => Box::new(OllamaProvider::new("llama-test", base).unwrap()),
            _ => unreachable!(),
        };

        assert_eq!(
            provider.chat("bounded prompt").await.unwrap(),
            expected_text
        );
        let request = request.await.unwrap();
        let first_line = request.head.lines().next().unwrap();
        assert_eq!(first_line, format!("POST {expected_target} HTTP/1.1"));
        assert!(request
            .head
            .to_ascii_lowercase()
            .contains("content-type: application/json"));
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["model"],
            if kind == "gemini" {
                Value::Null
            } else {
                json!((match kind {
                    "openai" => "gpt-test",
                    "claude" => "claude-test",
                    "ollama" => "llama-test",
                    _ => unreachable!(),
                })
                .to_string())
            }
        );
        let serialized = String::from_utf8(request.body).unwrap();
        assert!(serialized.contains("bounded prompt"));
        match kind {
            "openai" => assert!(
                request.head.contains("authorization: Bearer openai-secret")
                    || request.head.contains("Authorization: Bearer openai-secret")
            ),
            "claude" => {
                assert!(request
                    .head
                    .to_ascii_lowercase()
                    .contains("x-api-key: claude-secret"));
                assert!(request
                    .head
                    .to_ascii_lowercase()
                    .contains("anthropic-version: 2023-06-01"));
            }
            "gemini" | "ollama" => {}
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn provider_failures_have_stable_redacted_categories() {
    let cases = [
        (400, &[][..], ErrorCode::ProviderRequest),
        (401, &[][..], ErrorCode::Authentication),
        (403, &[][..], ErrorCode::Authentication),
        (429, &[("Retry-After", "17")][..], ErrorCode::RateLimited),
        (500, &[][..], ErrorCode::Upstream),
        (
            302,
            &[("Location", "http://secret.invalid/key")][..],
            ErrorCode::Redirect,
        ),
    ];
    for (status, headers, expected) in cases {
        let (base, _) =
            one_shot_server(json_response(status, "provider-secret-body", headers)).await;
        let provider = OpenAiProvider::new(
            "gpt-test",
            base.join("v1/chat/completions").unwrap(),
            "provider-secret-key",
        )
        .unwrap();
        let error = provider.chat("private prompt text").await.unwrap_err();
        assert_eq!(error.code(), expected);
        if status == 429 {
            assert_eq!(error.retry_after_secs(), Some(17));
        }
        let rendered = format!("{error:?} {error}");
        for secret in [
            "provider-secret-body",
            "provider-secret-key",
            "private prompt text",
            "secret.invalid",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
    }
}

#[tokio::test]
async fn malformed_empty_and_oversized_provider_responses_are_rejected() {
    let cases = [
        (
            json_response(200, "not-json", &[]),
            ErrorCode::MalformedResponse,
        ),
        (
            json_response(200, r#"{"choices":[{"message":{"content":"  "}}]}"#, &[]),
            ErrorCode::EmptyResponse,
        ),
        (
            json_response(
                200,
                &format!(
                    r#"{{"choices":[{{"message":{{"content":"{}"}}}}]}}"#,
                    "x".repeat(1024 * 1024 + 1)
                ),
                &[],
            ),
            ErrorCode::BodyLimit,
        ),
        (
            json_response(
                200,
                &format!(
                    r#"{{"choices":[{{"message":{{"content":"{}"}}}}]}}"#,
                    "🦀".repeat(70_000)
                ),
                &[],
            ),
            ErrorCode::BodyLimit,
        ),
    ];
    for (response, expected) in cases {
        let (base, _) = one_shot_server(response).await;
        let provider = OpenAiProvider::new("gpt-test", base.join("chat").unwrap(), "key").unwrap();
        assert_eq!(provider.chat("prompt").await.unwrap_err().code(), expected);
    }
}

#[test]
fn provider_debug_never_exposes_keys_models_or_endpoints() {
    let provider = GeminiProvider::new(
        "private-model",
        url::Url::parse("https://example.invalid/v1beta/?endpoint-secret=yes").unwrap(),
        "private-key",
    )
    .unwrap_err();
    let rendered = format!("{provider:?}");
    assert!(!rendered.contains("private-model"));
    assert!(!rendered.contains("private-key"));
    assert!(!rendered.contains("endpoint-secret"));

    let oversized_endpoint =
        url::Url::parse(&format!("https://example.invalid/{}", "a".repeat(33_000))).unwrap();
    assert_eq!(
        OllamaProvider::new("model", oversized_endpoint)
            .unwrap_err()
            .code(),
        ErrorCode::InvalidInput
    );
}

#[tokio::test(start_paused = true)]
async fn provider_request_has_a_sixty_second_absolute_timeout() {
    let base = hanging_server().await;
    let provider = OpenAiProvider::new("gpt-test", base.join("chat").unwrap(), "key").unwrap();
    let start = tokio::time::Instant::now();
    let error = provider.chat("prompt").await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Timeout);
    assert!(start.elapsed() >= std::time::Duration::from_secs(60));
}

#[tokio::test]
async fn provider_connection_failures_and_multibyte_prompt_caps_are_stable() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let provider = OpenAiProvider::new(
        "gpt-test",
        url::Url::parse(&format!("http://{address}/chat")).unwrap(),
        "key",
    )
    .unwrap();
    assert_eq!(
        provider.chat("prompt").await.unwrap_err().code(),
        ErrorCode::Connection
    );
    assert_eq!(
        provider
            .chat(&"🦀".repeat(70_000))
            .await
            .unwrap_err()
            .code(),
        ErrorCode::InvalidInput
    );
}
