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

pub mod chatgpt;
pub mod claude;
pub mod deepseek;
pub mod file_upload;
pub mod gemini;
pub mod glm;
pub mod grok;
pub mod kimi;
pub mod metaai;
pub mod minimax;
pub mod mimo;
pub mod mistral;
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

/// Signal that the response stream has finished.
///
/// Providers describe how `chat::run_chat` decides a generation is complete.
/// Each variant maps to a different observable signal on the page.
#[derive(Debug, Clone, Copy)]
pub enum DoneSignal {
    /// Done when the response container's visible text has been stable
    /// (no growth) for the given duration.
    ///
    /// Best for chat UIs that stop appending text when generation finishes.
    /// Pair with a duration comfortably larger than one render frame
    /// (e.g. 1.0–2.0 s) so a slow token doesn't prematurely terminate.
    TextStable(Duration),

    /// Done when the given CSS selector no longer matches any visible
    /// element.
    ///
    /// Best for chat UIs that show a "Stop generating" / "Regenerate"
    /// affordance that disappears or appears when the stream ends.
    #[allow(dead_code)]
    SelectorDisappears(&'static str),
}

/// How a provider satisfies a chat completion request.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum ChatMode {
    /// Drive the provider's web UI: fill input, click submit, poll the DOM.
    Ui,

    /// Bypass the UI and call provider backend APIs directly from Rust,
    /// using the warmed browser only for authenticated state and any
    /// in-page computation (e.g. WASM PoW solving).
    Direct,
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

    /// How this provider satisfies chat completions.
    fn chat_mode(&self) -> ChatMode {
        ChatMode::Ui
    }

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

    /// CSS selectors for the chat input. Tried in order; first match wins.
    fn input_selectors(&self) -> &'static [&'static str];

    /// CSS selectors for the submit button. Tried in order; first match wins.
    fn submit_selectors(&self) -> &'static [&'static str];

    /// Selector for the assistant's last response container.
    ///
    /// The gateway extracts visible text from this subtree on every poll.
    fn response_selector(&self) -> &'static str;

    /// Optional selector for the reasoning / chain-of-thought panel.
    ///
    /// Returned text is routed to the `reasoning_content` delta in OpenAI
    /// streaming responses. Return `None` for providers without a visible
    /// reasoning panel.
    fn thinking_selector(&self) -> Option<&'static str>;

    /// How the gateway decides a generation has finished.
    fn done_signal(&self) -> DoneSignal;

    /// Optional synchronous JS expression to run on the warmed page
    /// immediately before the prompt is filled in. Used for "New Chat",
    /// model-picker clicks, etc.
    ///
    /// Must be fully synchronous — no Promises, no `setTimeout`. Returns
    /// ignored; surface errors via `throw new Error(...)`.
    fn pre_prompt_js(&self) -> Option<&'static str> {
        None
    }

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
            sessions: &SessionManager,
            _state: &AppState,
            request: ChatCompletionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>> {
            let sessions = sessions.clone();
            let provider = self.clone();
            Box::pin(async move { crate::chat::chat(&sessions, &provider, request).await })
        }

        fn chat_stream(
            &self,
            sessions: &SessionManager,
            _state: &AppState,
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
        > {
            let sessions = sessions.clone();
            let provider = self.clone();
            Box::pin(async move {
                crate::chat::chat_stream(&sessions, Arc::new(provider), request).await
            })
        }

        fn input_selectors(&self) -> &'static [&'static str] {
            &["textarea"]
        }

        fn submit_selectors(&self) -> &'static [&'static str] {
            &["button[type='submit']"]
        }

        fn response_selector(&self) -> &'static str {
            ".response"
        }

        fn thinking_selector(&self) -> Option<&'static str> {
            None
        }

        fn done_signal(&self) -> DoneSignal {
            DoneSignal::TextStable(Duration::from_millis(500))
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
        assert_eq!(p.input_selectors(), &["textarea"]);
        assert_eq!(p.submit_selectors(), &["button[type='submit']"]);
        assert_eq!(p.response_selector(), ".response");
        assert!(p.thinking_selector().is_none());
        assert!(matches!(
            p.done_signal(),
            DoneSignal::TextStable(_) | DoneSignal::SelectorDisappears(_)
        ));
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
