use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use rand::Rng;

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model};
use crate::session::SessionManager;
use crate::state::AppState;

pub mod authcheck;
pub mod chatgpt;
pub mod claude;
pub mod deepseek;
pub mod file_upload;
pub mod gemini;
pub mod glm;
pub mod grok;
pub mod health;
pub mod kimi;
pub mod manifest;
pub mod metaai;
pub mod minimax;
pub mod mimo;
pub mod mistral;
pub mod mtp;
pub mod profile;
pub mod qwen;
pub mod session_guard;
pub mod session_store;
pub mod solver;
pub mod streaming_upload;
pub mod tokenizer;
pub mod tool_call;

/// Maximum number of retry attempts for transient HTTP failures.
const MAX_RETRY_ATTEMPTS: u32 = 3;

/// Reject `response_format: {"type":"json_object"}` requests.
///
/// JSON mode is a native field of the paid OpenAI/DeepSeek APIs. None of the
/// free web-UI internal endpoints this gateway talks to expose a native
/// JSON-mode channel, so honouring it would require prompt injection. Fail
/// closed instead of faking it. Every provider's `validate_request` must call
/// this first.
pub(crate) fn validate_no_native_json_mode(
    model: &str,
    request: &ChatCompletionRequest,
) -> Result<(), GatewayError> {
    if let Some(fmt) = &request.response_format {
        if fmt.r#type == "json_object" {
            return Err(GatewayError::BadRequest(format!(
                "model '{model}' does not support response_format \"json_object\": \
                 its web API has no native JSON-mode channel"
            )));
        }
    }
    Ok(())
}

/// Backoff delay for the given attempt number (1-indexed).
/// 1s, 2s, 4s with up to 500ms of jitter.
fn backoff(attempt: u32) -> Duration {
    let base = 1000_u64.saturating_mul(2_u64.saturating_pow(attempt.saturating_sub(1)));
    let jitter = rand::thread_rng().gen_range(0..500_u64);
    Duration::from_millis(base + jitter)
}

/// Returns true if the HTTP status code is worth retrying.
fn is_retryable_status(code: u16) -> bool {
    code == 429 || code >= 500
}

/// Parse a `Retry-After` header value: either a bare number of seconds
/// (per RFC 9110 the common form) or an HTTP-date. The date form is not
/// implemented; `Some` is only produced for plain seconds, matching what
/// the providers this gateway talks to actually send.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Extract `Retry-After` from a `HashMap`-shaped header map (stealth
/// client responses carry headers this way, keys lowercased).
pub fn retry_after_from_map(headers: &HashMap<String, String>) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|v| parse_retry_after(v))
}

/// Extract `Retry-After` from a reqwest response header map.
pub fn retry_after_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_retry_after)
}

/// Send an HTTP request with automatic retries on transient failures.
///
/// Retries on transport errors (timeout, connection refused) and on
/// 429 (rate limit) / 5xx (server errors). After the retry budget is
/// exhausted, the most recent response is returned regardless of
/// status so callers can inspect the body for a useful error message.
/// Only returns `Err` when transport errors persist beyond retries.
///
/// The builder is retried via `RequestBuilder::try_clone()`, which
/// requires the body to be clonable (JSON works; streaming does not).
pub async fn send_with_retry(
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::Response, GatewayError> {
    let mut last_response: Option<reqwest::Response> = None;
    let mut last_transport_error: Option<reqwest::Error> = None;

    for attempt in 1..=MAX_RETRY_ATTEMPTS {
        let req = builder.try_clone().ok_or_else(|| {
            GatewayError::Internal("request body cannot be cloned for retry".to_string())
        })?;

        match req.send().await {
            Ok(resp) => {
                let code = resp.status().as_u16();
                if is_retryable_status(code) {
                    last_response = Some(resp);
                    if attempt < MAX_RETRY_ATTEMPTS {
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    continue;
                }
                // Success or non-retryable error — return the response.
                return Ok(resp);
            }
            Err(e) => {
                last_transport_error = Some(e);
                if attempt < MAX_RETRY_ATTEMPTS {
                    tokio::time::sleep(backoff(attempt)).await;
                }
            }
        }
    }

    // Out of retries. Prefer the most informative error.
    if let Some(resp) = last_response {
        return Ok(resp);
    }
    let detail = last_transport_error
        .map(|e| format!("transport error after {MAX_RETRY_ATTEMPTS} attempts: {e}"))
        .unwrap_or_else(|| format!("failed after {MAX_RETRY_ATTEMPTS} attempts"));
    Err(GatewayError::Internal(detail))
}

/// A web-AI provider adapter.
///
/// Implementations describe **how to find and read** the AI's response on
/// a specific provider's web UI (selectors, done signal, URL). The actual
/// driving of the browser — filling input, clicking submit, polling for
/// text — lives in [`crate::chat`] and is shared by every provider.
///
/// The gateway owns the loop. The provider only declares selectors and
/// configuration. Adding a new provider is a small struct with a handful
/// of `&'static str` constants.
///
/// The trait is object-safe: chat/chat_stream return boxed futures so they
/// can be dispatched through `Arc<dyn Provider>`.
#[allow(dead_code)]
pub trait Provider: Send + Sync {
    /// Provider identifier (e.g. "deepseek").
    fn name(&self) -> &'static str;

    /// URL the warmed page should be navigated to before the first request.
    fn url(&self) -> &'static str;

    /// Models this provider exposes via the OpenAI `/v1/models` endpoint.
    fn models(&self) -> Vec<Model>;

    /// Non-streaming chat completion.
    fn chat(
        &self,
        sessions: &SessionManager,
        state: &AppState,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>>;

    /// Streaming chat completion.
    fn chat_stream(
        &self,
        sessions: &SessionManager,
        state: &AppState,
        request: ChatCompletionRequest,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
                        GatewayError,
                    >,
                > + Send,
        >,
    >;

    /// Whether this adapter can safely accept OpenAI image/file URL content
    /// parts. UI-backed providers must opt in only after their upload flow is
    /// implemented and exercised against the live UI.
    fn supports_attachments(&self) -> bool {
        true
    }

    /// Reject requests that a provider cannot honour. The old direct adapters
    /// retain their existing behavior; browser UI adapters deliberately fail
    /// closed instead of silently dropping unsupported controls.
    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        let has_attachments = request.messages.iter().any(|message| {
            !message.content.image_urls().is_empty() || !message.content.file_urls().is_empty()
        });
        if has_attachments && !self.supports_attachments() {
            return Err(GatewayError::BadRequest(format!(
                "model '{}' does not support file or image attachments through its web UI yet",
                request.model
            )));
        }

        validate_no_native_json_mode(&request.model, request)?;

        Ok(())
    }
}

/// Registry mapping model IDs to provider implementations.
///
/// Model IDs are expected to be unique across providers. The registry
/// enforces this at registration time.
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn Provider>>,
    model_to_provider: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
            model_to_provider: HashMap::new(),
        }
    }

    /// Register a provider and all of its models.
    ///
    /// Returns an error if any model ID is already registered by another
    /// provider.
    pub fn register(&mut self, provider: Arc<dyn Provider>) -> Result<(), GatewayError> {
        for model in provider.models() {
            if self.model_to_provider.contains_key(&model.id) {
                return Err(GatewayError::Config(format!(
                    "model '{}' is registered by more than one provider",
                    model.id
                )));
            }
            self.model_to_provider
                .insert(model.id.clone(), provider.clone());
        }
        self.providers.push(provider);
        Ok(())
    }

    /// Lookup a provider by model ID.
    pub fn get(&self, model_id: &str) -> Option<Arc<dyn Provider>> {
        self.model_to_provider.get(model_id).cloned()
    }

    /// Check whether a model ID exists.
    #[allow(dead_code)]
    pub fn has_model(&self, model_id: &str) -> bool {
        self.model_to_provider.contains_key(model_id)
    }

    /// All models from all registered providers.
    pub fn all_models(&self) -> Vec<Model> {
        self.providers.iter().flat_map(|p| p.models()).collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ResponseFormat;

    #[test]
    fn parse_retry_after_seconds() {
        assert_eq!(parse_retry_after("30"), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
        assert_eq!(parse_retry_after("Fri, 31 Dec 1999 23:59:59 GMT"), None);
        assert_eq!(parse_retry_after("abc"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn retry_after_from_map_and_headers() {
        let mut map = HashMap::new();
        assert_eq!(retry_after_from_map(&map), None);
        map.insert("retry-after".to_string(), "45".to_string());
        assert_eq!(retry_after_from_map(&map), Some(Duration::from_secs(45)));

        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_from_headers(&headers), None);
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("12"),
        );
        assert_eq!(
            retry_after_from_headers(&headers),
            Some(Duration::from_secs(12))
        );
    }

    #[test]
    fn rate_limit_error_carries_retry_after_header() {
        let err = GatewayError::ProviderRateLimited {
            message: "slow down".to_string(),
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(err.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn plain_errors_have_no_retry_after() {
        let err = GatewayError::Provider("boom".to_string());
        assert_eq!(err.retry_after(), None);
    }

    #[derive(Clone)]
    struct DummyProvider;

    impl Provider for DummyProvider {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn url(&self) -> &'static str {
            "https://example.com"
        }

        fn models(&self) -> Vec<Model> {
            vec![Model {
                id: "dummy-model".to_string(),
                object: "model".to_string(),
                created: 1,
                owned_by: "dummy".to_string(),
            }]
        }

        fn chat(
            &self,
            _sessions: &SessionManager,
            _state: &AppState,
            _request: ChatCompletionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>> {
            Box::pin(async {
                Err(GatewayError::Internal(
                    "DummyProvider does not support chat".to_string(),
                ))
            })
        }

        fn chat_stream(
            &self,
            _sessions: &SessionManager,
            _state: &AppState,
            _request: ChatCompletionRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
                            GatewayError,
                        >,
                    > + Send,
            >,
        > {
            Box::pin(async {
                Err(GatewayError::Internal(
                    "DummyProvider does not support streaming".to_string(),
                ))
            })
        }
    }

    #[test]
    fn registry_finds_provider_by_model() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(DummyProvider)).unwrap();
        assert!(registry.has_model("dummy-model"));
        assert!(registry.get("dummy-model").is_some());
        assert!(!registry.has_model("missing"));
    }

    #[test]
    fn registry_rejects_duplicate_models() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(DummyProvider)).unwrap();
        assert!(registry.register(Arc::new(DummyProvider)).is_err());
    }

    #[test]
    fn dummy_provider_exposes_config() {
        let p = DummyProvider;
        assert_eq!(p.name(), "dummy");
        assert_eq!(p.url(), "https://example.com");
        assert!(p.supports_attachments());
    }

    #[test]
    fn validate_no_native_json_mode_rejects_json_object() {
        let mut request = ChatCompletionRequest {
            model: "dummy-model".to_string(),
            messages: vec![],
            stream: false,
            session_url: None,
            thinking: None,
            search: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            user: None,
            response_format: None,
        };
        // `text` is fine.
        request.response_format = Some(ResponseFormat {
            r#type: "text".to_string(),
        });
        assert!(validate_no_native_json_mode("dummy-model", &request).is_ok());
        // `json_object` must fail closed.
        request.response_format = Some(ResponseFormat {
            r#type: "json_object".to_string(),
        });
        let err = validate_no_native_json_mode("dummy-model", &request).unwrap_err();
        assert!(err.to_string().contains("json_object"));
    }

    #[test]
    fn retry_status_classification() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(504));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(422));
    }

    #[test]
    fn backoff_increases_with_attempts() {
        let d1 = backoff(1);
        let d2 = backoff(2);
        let d3 = backoff(3);
        // Lower bounds: 1s, 2s, 4s minus jitter (max 500ms)
        assert!(d1 >= Duration::from_millis(1000));
        assert!(d2 >= Duration::from_millis(2000));
        assert!(d3 >= Duration::from_millis(4000));
        // Upper bounds: 1s, 2s, 4s + 500ms jitter
        assert!(d1 < Duration::from_millis(1500));
        assert!(d2 < Duration::from_millis(2500));
        assert!(d3 < Duration::from_millis(4500));
    }

    /// Spawn a tiny HTTP server that returns a sequence of status codes.
    /// Used to exercise `send_with_retry` end-to-end.
    async fn spawn_mock_server(
        responses: Vec<u16>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let counter = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/test");

        let counter_clone = counter.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };

                let idx = counter_clone.fetch_add(1, Ordering::SeqCst);
                let code = if idx < responses.len() {
                    responses[idx]
                } else {
                    *responses.last().unwrap_or(&500)
                };

                // Spawn a task per connection so we don't block the accept loop
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;

                    let body = format!("attempt {idx} -> {code}");
                    let reason = match code {
                        200 => "OK",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        502 => "Bad Gateway",
                        503 => "Service Unavailable",
                        504 => "Gateway Timeout",
                        _ => "Status",
                    };
                    let response = format!(
                        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        (url, counter)
    }

    #[tokio::test]
    async fn send_with_retry_succeeds_after_503s() {
        let (url, counter) = spawn_mock_server(vec![503, 503, 200]).await;
        let client = reqwest::Client::new();
        let builder = client.get(&url);
        let resp = send_with_retry(builder).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        // Should have made 3 attempts
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn send_with_retry_returns_last_response_after_exhausting_retries() {
        let (url, counter) = spawn_mock_server(vec![503, 503, 503]).await;
        let client = reqwest::Client::new();
        let builder = client.get(&url);
        // After 3 attempts of 503, returns the last 503 response
        let resp = send_with_retry(builder).await.unwrap();
        assert_eq!(resp.status().as_u16(), 503);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn send_with_retry_no_retry_on_400() {
        let (url, counter) = spawn_mock_server(vec![400, 200]).await;
        let client = reqwest::Client::new();
        let builder = client.get(&url);
        let resp = send_with_retry(builder).await.unwrap();
        // Returns immediately with the 400
        assert_eq!(resp.status().as_u16(), 400);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn send_with_retry_retries_on_429() {
        let (url, counter) = spawn_mock_server(vec![429, 200]).await;
        let client = reqwest::Client::new();
        let builder = client.get(&url);
        let resp = send_with_retry(builder).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
