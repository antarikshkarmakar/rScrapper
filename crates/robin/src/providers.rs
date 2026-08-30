//! Typed, bounded provider adapters.

use crate::{
    contains_forbidden_controls, validate_generated_text, validate_model, validate_url_bound,
    Error, ErrorCode, Provider, Result, MAX_PROMPT_BYTES, MAX_PROMPT_CHARS,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROVIDER_BODY_BYTES: usize = 1024 * 1024;
const MAX_KEY_CHARS: usize = 4_096;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, prompt: &str) -> Result<String>;
}

struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone, Copy)]
enum ProviderKind {
    OpenAi,
    Claude,
    Gemini,
    Ollama,
}

impl ProviderKind {
    fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Ollama => "ollama",
        }
    }
}

struct ProviderClient {
    kind: ProviderKind,
    model: String,
    endpoint: Url,
    key: Option<Secret>,
    client: reqwest::Client,
}

impl fmt::Debug for ProviderClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderClient")
            .field("kind", &self.kind.name())
            .field("model", &"<redacted>")
            .field("endpoint", &"<redacted>")
            .field("key_configured", &self.key.is_some())
            .finish()
    }
}

impl ProviderClient {
    fn new(
        kind: ProviderKind,
        model: impl AsRef<str>,
        endpoint: Url,
        key: Option<impl AsRef<str>>,
    ) -> Result<Self> {
        let model = validate_model(model.as_ref())?;
        validate_endpoint(&endpoint)?;
        let key = key
            .map(|value| validate_key(value.as_ref()))
            .transpose()?
            .map(Secret);
        let client = reqwest::Client::builder()
            .no_proxy()
            .tls_backend_rustls()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(PROVIDER_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::new(ErrorCode::Configuration, "provider transport"))?;
        Ok(Self {
            kind,
            model,
            endpoint,
            key,
            client,
        })
    }

    async fn send<T: Serialize + ?Sized>(
        &self,
        url: Url,
        headers: HeaderMap,
        body: &T,
    ) -> Result<Vec<u8>> {
        validate_prompt_url(&url)?;
        let future = async {
            let response = self
                .client
                .post(url)
                .headers(headers)
                .json(body)
                .send()
                .await
                .map_err(map_request_error)?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(map_status(status, response.headers()));
            }
            if response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > MAX_PROVIDER_BODY_BYTES)
            {
                return Err(Error::new(ErrorCode::BodyLimit, "provider response")
                    .with_limit(MAX_PROVIDER_BODY_BYTES));
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(map_request_error)?;
                if chunk.len() > MAX_PROVIDER_BODY_BYTES.saturating_sub(bytes.len()) {
                    return Err(Error::new(ErrorCode::BodyLimit, "provider response")
                        .with_limit(MAX_PROVIDER_BODY_BYTES));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        };
        tokio::time::timeout(PROVIDER_TIMEOUT, future)
            .await
            .map_err(|_| Error::new(ErrorCode::Timeout, "provider request"))?
    }
}

pub struct OpenAiProvider(ProviderClient);
pub struct ClaudeProvider(ProviderClient);
pub struct GeminiProvider(ProviderClient);
pub struct OllamaProvider(ProviderClient);

macro_rules! redacted_provider_debug {
    ($type:ty, $name:literal) => {
        impl fmt::Debug for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct($name)
                    .field("configuration", &"<redacted>")
                    .finish()
            }
        }
    };
}

redacted_provider_debug!(OpenAiProvider, "OpenAiProvider");
redacted_provider_debug!(ClaudeProvider, "ClaudeProvider");
redacted_provider_debug!(GeminiProvider, "GeminiProvider");
redacted_provider_debug!(OllamaProvider, "OllamaProvider");

impl OpenAiProvider {
    pub fn new(model: impl AsRef<str>, endpoint: Url, key: impl AsRef<str>) -> Result<Self> {
        Ok(Self(ProviderClient::new(
            ProviderKind::OpenAi,
            model,
            endpoint,
            Some(key),
        )?))
    }
}

impl ClaudeProvider {
    pub fn new(model: impl AsRef<str>, endpoint: Url, key: impl AsRef<str>) -> Result<Self> {
        Ok(Self(ProviderClient::new(
            ProviderKind::Claude,
            model,
            endpoint,
            Some(key),
        )?))
    }
}

impl GeminiProvider {
    pub fn new(model: impl AsRef<str>, endpoint: Url, key: impl AsRef<str>) -> Result<Self> {
        Ok(Self(ProviderClient::new(
            ProviderKind::Gemini,
            model,
            endpoint,
            Some(key),
        )?))
    }
}

impl OllamaProvider {
    pub fn new(model: impl AsRef<str>, endpoint: Url) -> Result<Self> {
        Ok(Self(ProviderClient::new(
            ProviderKind::Ollama,
            model,
            endpoint,
            Option::<&str>::None,
        )?))
    }
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: [Message<'a>; 1],
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}

#[async_trait]
impl ChatProvider for OpenAiProvider {
    async fn chat(&self, prompt: &str) -> Result<String> {
        validate_prompt(prompt)?;
        let key = self
            .0
            .key
            .as_ref()
            .ok_or_else(|| Error::new(ErrorCode::Configuration, "OpenAI key"))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", key.0))
                .map_err(|_| Error::new(ErrorCode::InvalidInput, "OpenAI key"))?,
        );
        let bytes = self
            .0
            .send(
                self.0.endpoint.clone(),
                headers,
                &OpenAiRequest {
                    model: &self.0.model,
                    messages: [Message {
                        role: "user",
                        content: prompt,
                    }],
                },
            )
            .await?;
        let response: OpenAiResponse = parse_json(&bytes)?;
        extract_text(
            response
                .choices
                .into_iter()
                .next()
                .map(|choice| choice.message.content),
        )
    }
}

#[derive(Serialize)]
struct ClaudeRequest<'a> {
    model: &'a str,
    max_tokens: u16,
    messages: [Message<'a>; 1],
}

#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContent>,
}

#[derive(Deserialize)]
struct ClaudeContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[async_trait]
impl ChatProvider for ClaudeProvider {
    async fn chat(&self, prompt: &str) -> Result<String> {
        validate_prompt(prompt)?;
        let key = self
            .0
            .key
            .as_ref()
            .ok_or_else(|| Error::new(ErrorCode::Configuration, "Anthropic key"))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&key.0)
                .map_err(|_| Error::new(ErrorCode::InvalidInput, "Anthropic key"))?,
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        let bytes = self
            .0
            .send(
                self.0.endpoint.clone(),
                headers,
                &ClaudeRequest {
                    model: &self.0.model,
                    max_tokens: 1_024,
                    messages: [Message {
                        role: "user",
                        content: prompt,
                    }],
                },
            )
            .await?;
        let response: ClaudeResponse = parse_json(&bytes)?;
        let content = response
            .content
            .into_iter()
            .find(|content| content.kind == "text")
            .and_then(|content| content.text);
        extract_text(content)
    }
}

#[derive(Serialize)]
struct GeminiRequest<'a> {
    contents: [GeminiContent<'a>; 1],
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    parts: [GeminiPart<'a>; 1],
}

#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiResponseContent,
}

#[derive(Deserialize)]
struct GeminiResponseContent {
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    text: Option<String>,
}

#[async_trait]
impl ChatProvider for GeminiProvider {
    async fn chat(&self, prompt: &str) -> Result<String> {
        validate_prompt(prompt)?;
        let key = self
            .0
            .key
            .as_ref()
            .ok_or_else(|| Error::new(ErrorCode::Configuration, "Gemini key"))?;
        let mut url = self.0.endpoint.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| Error::new(ErrorCode::InvalidInput, "Gemini endpoint"))?;
            segments.pop_if_empty();
            segments.push("models");
            segments.push(&format!("{}:generateContent", self.0.model));
        }
        url.query_pairs_mut().append_pair("key", &key.0);
        let bytes = self
            .0
            .send(
                url,
                HeaderMap::new(),
                &GeminiRequest {
                    contents: [GeminiContent {
                        parts: [GeminiPart { text: prompt }],
                    }],
                },
            )
            .await?;
        let response: GeminiResponse = parse_json(&bytes)?;
        let text = response
            .candidates
            .into_iter()
            .next()
            .and_then(|candidate| {
                candidate
                    .content
                    .parts
                    .into_iter()
                    .find_map(|part| part.text)
            });
        extract_text(text)
    }
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    stream: bool,
    messages: [Message<'a>; 1],
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: OpenAiMessage,
}

#[async_trait]
impl ChatProvider for OllamaProvider {
    async fn chat(&self, prompt: &str) -> Result<String> {
        validate_prompt(prompt)?;
        let url = self
            .0
            .endpoint
            .join("api/chat")
            .map_err(|_| Error::new(ErrorCode::InvalidInput, "Ollama endpoint"))?;
        let bytes = self
            .0
            .send(
                url,
                HeaderMap::new(),
                &OllamaRequest {
                    model: &self.0.model,
                    stream: false,
                    messages: [Message {
                        role: "user",
                        content: prompt,
                    }],
                },
            )
            .await?;
        let response: OllamaResponse = parse_json(&bytes)?;
        extract_text(Some(response.message.content))
    }
}

pub fn from_environment(provider: &Provider) -> Result<Box<dyn ChatProvider>> {
    match provider {
        Provider::OpenAI { model } => Ok(Box::new(OpenAiProvider::new(
            model,
            static_url("https://api.openai.com/v1/chat/completions")?,
            required_environment("OPENAI_API_KEY")?,
        )?)),
        Provider::Claude { model } => Ok(Box::new(ClaudeProvider::new(
            model,
            static_url("https://api.anthropic.com/v1/messages")?,
            required_environment("ANTHROPIC_API_KEY")?,
        )?)),
        Provider::Gemini { model } => Ok(Box::new(GeminiProvider::new(
            model,
            static_url("https://generativelanguage.googleapis.com/v1beta/")?,
            required_environment("GEMINI_API_KEY")?,
        )?)),
        Provider::Ollama { model } => {
            let endpoint =
                std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434/".into());
            let endpoint = Url::parse(&endpoint)
                .map_err(|_| Error::new(ErrorCode::Configuration, "Ollama host"))?;
            Ok(Box::new(OllamaProvider::new(model, endpoint)?))
        }
    }
}

fn validate_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty()
        || prompt.chars().count() > MAX_PROMPT_CHARS
        || prompt.len() > MAX_PROMPT_BYTES
        || contains_forbidden_controls(prompt)
    {
        return Err(
            Error::new(ErrorCode::InvalidInput, "provider prompt").with_limit(MAX_PROMPT_CHARS)
        );
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS || key.chars().any(char::is_control) {
        return Err(Error::new(ErrorCode::Configuration, "provider key"));
    }
    Ok(key.to_owned())
}

fn validate_endpoint(endpoint: &Url) -> Result<()> {
    validate_url_bound("provider endpoint", endpoint)?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(Error::new(ErrorCode::InvalidInput, "provider endpoint"));
    }
    Ok(())
}

fn validate_prompt_url(url: &Url) -> Result<()> {
    validate_url_bound("provider request URL", url)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::new(ErrorCode::InvalidInput, "provider request URL"));
    }
    Ok(())
}

fn map_status(status: u16, headers: &HeaderMap) -> Error {
    match status {
        400 => Error::new(ErrorCode::ProviderRequest, "provider status"),
        401 | 403 => Error::new(ErrorCode::Authentication, "provider status"),
        429 => Error::new(ErrorCode::RateLimited, "provider status").with_retry_after(
            headers
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse().ok()),
        ),
        300..=399 => Error::new(ErrorCode::Redirect, "provider status"),
        _ => Error::new(ErrorCode::Upstream, "provider status"),
    }
}

fn map_request_error(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::new(ErrorCode::Timeout, "provider request")
    } else {
        Error::new(ErrorCode::Connection, "provider request")
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|_| Error::new(ErrorCode::MalformedResponse, "provider JSON"))
}

fn extract_text(text: Option<String>) -> Result<String> {
    let text = text.ok_or_else(|| Error::new(ErrorCode::EmptyResponse, "provider content"))?;
    validate_generated_text(&text)
}

fn required_environment(name: &'static str) -> Result<String> {
    std::env::var(name).map_err(|_| Error::new(ErrorCode::Configuration, "provider key"))
}

fn static_url(value: &'static str) -> Result<Url> {
    Url::parse(value).map_err(|_| Error::new(ErrorCode::Configuration, "provider endpoint"))
}

#[allow(dead_code)]
fn _assert_provider_clients_are_shareable() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<OpenAiProvider>();
    assert_send_sync::<ClaudeProvider>();
    assert_send_sync::<GeminiProvider>();
    assert_send_sync::<OllamaProvider>();
    let _: Option<Arc<dyn ChatProvider>> = None;
}
