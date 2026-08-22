use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model};
use crate::providers::session_guard::SessionGuardStream;
use crate::providers::Provider;
use crate::session::SessionManager;
use crate::state::AppState;

use direct::DirectClient;
use state::MinimaxSessionStore;

mod direct;
mod state;
mod upload;

/// How long an account-level quota block is cached. MiniMax's Token Plan quota
/// resets on a 5-hour rolling window, so a 30-minute cache avoids wasting ~8s
/// per call on a known-dead account while still recovering within the hour if
/// the account gets Credits or the window resets.
const QUOTA_BLOCK_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
pub struct MinimaxProvider {
    store: MinimaxSessionStore,
    quota_block: Arc<Mutex<Option<QuotaBlock>>>,
}

#[derive(Clone)]
struct QuotaBlock {
    until: Instant,
    message: String,
}

/// Detect MiniMax account-level Token Plan quota exhaustion from an upstream
/// error message. MiniMax surfaces this in-band (finish_reason: error) with the
/// codes 42212 (archon message) and 2056 (platform), plus the billing-specific
/// "Token Plan" wording. Per MiniMax docs this is NOT retry-able, so the
/// provider caches it and fails fast.
fn is_minimax_quota_exhaustion(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("2056") || m.contains("42212") || m.contains("token plan")
}

/// Append MiniMax's documented remediation for the quota error so API users
/// know the account is out of quota and what to do, instead of seeing a bare
/// upstream message.
fn augment_quota_message(msg: &str) -> String {
    format!(
        "{msg} (MiniMax Token Plan quota exhausted. This is not retry-able: wait \
         for the 5-hour rolling / weekly window to reset, upgrade the Token Plan, \
         purchase Credits, or claim daily free credits in MiniMax Code at \
         agent.minimax.io.)"
    )
}

impl MinimaxProvider {
    pub fn new() -> Self {
        Self {
            store: MinimaxSessionStore::new(),
            quota_block: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a provider with optional disk-persisted sessions.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            store: MinimaxSessionStore::with_data_dir(data_dir),
            quota_block: Arc::new(Mutex::new(None)),
        }
    }

    /// If the account is known to be out of quota, return the cached error to
    /// fail fast instead of wasting a session + upstream round-trip.
    async fn quota_blocked_message(&self) -> Option<String> {
        let guard = self.quota_block.lock().await;
        match guard.as_ref() {
            Some(block) if Instant::now() < block.until => Some(block.message.clone()),
            _ => None,
        }
    }

    /// Cache the quota error (augmented with remediation) so subsequent calls
    /// fail fast.
    async fn set_quota_block(&self, message: String) {
        let msg = if is_minimax_quota_exhaustion(&message) {
            augment_quota_message(&message)
        } else {
            message
        };
        let mut guard = self.quota_block.lock().await;
        *guard = Some(QuotaBlock {
            until: Instant::now() + QUOTA_BLOCK_TTL,
            message: msg,
        });
    }

    /// Record a quota block from a `GatewayError` if the error indicates
    /// account-level exhaustion. No-op otherwise.
    async fn set_quota_block_if_exhausted(&self, err: &GatewayError) {
        if let GatewayError::Provider(msg) = err {
            if is_minimax_quota_exhaustion(msg) {
                self.set_quota_block(msg.clone()).await;
            }
        }
    }
}

impl Default for MinimaxProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MinimaxProvider {
    fn name(&self) -> &'static str {
        "minimax"
    }

    fn url(&self) -> &'static str {
        "https://agent.minimax.io"
    }

    fn models(&self) -> Vec<Model> {
        vec![
            Model {
                id: "minimax-m3".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "minimax".to_string(),
            },
            Model {
                id: "minimax-m2.7".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "minimax".to_string(),
            },
            Model {
                id: "minimax-m2.7-highspeed".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "minimax".to_string(),
            },
        ]
    }


    fn supports_attachments(&self) -> bool {
        true
    }

    fn chat(
        &self,
        sessions: &SessionManager,
        _state: &AppState,
        request: ChatCompletionRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>,
    > {
        let this = self.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            if let Some(msg) = this.quota_blocked_message().await {
                return Err(GatewayError::Provider(msg));
            }
            let session = sessions.acquire().await?;
            let client = match DirectClient::new(
                session.clone(),
                &request.model,
                this.store.clone(),
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = sessions.release(session.id, false).await;
                    return Err(e);
                }
            };
            let result = client.chat(request, &session.id).await;
            if let Err(ref e) = result {
                this.set_quota_block_if_exhausted(e).await;
            }
            let _ = sessions.release(session.id.clone(), false).await;
            result
        })
    }

    fn chat_stream(
        &self,
        sessions: &SessionManager,
        _state: &AppState,
        request: ChatCompletionRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
                        GatewayError,
                    >,
                > + Send,
        >,
    > {
        let this = self.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            if let Some(msg) = this.quota_blocked_message().await {
                return Err(GatewayError::Provider(msg));
            }

            let max_attempts = 2;
            let mut last_error: Option<GatewayError> = None;
            let store = this.store.clone();

            for _attempt in 1..=max_attempts {
                let session = match sessions.acquire().await {
                    Ok(s) => s,
                    Err(e) => {
                        last_error = Some(e);
                        continue;
                    }
                };

                let client = match DirectClient::new(
                    session.clone(),
                    &request.model,
                    store.clone(),
                )
                .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = sessions.release(session.id, false).await;
                        last_error = Some(e);
                        continue;
                    }
                };

                let session_id = session.id.clone();
                let sessions_for_stream = sessions.clone();

                let stream = match client.chat_stream(request.clone(), &session_id).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = sessions.release(session_id, false).await;
                        last_error = Some(e);
                        continue;
                    }
                };

                let (tx, rx) = mpsc::unbounded_channel();

                let this_relay = this.clone();
                tokio::spawn(async move {
                    let mut stream = stream;
                    while let Some(chunk) = stream.next().await {
                        if let Err(ref e) = chunk {
                            this_relay.set_quota_block_if_exhausted(e).await;
                        }
                        let _ = tx.send(chunk);
                    }
                });

                let guarded = SessionGuardStream::new(
                    UnboundedReceiverStream::new(rx).boxed(),
                    sessions_for_stream,
                    session_id,
                );
                return Ok(guarded.boxed());
            }

            Err(last_error.unwrap_or_else(|| {
                GatewayError::Provider("Minimax: failed to acquire session".to_string())
            }))
        })
    }






    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // Minimax web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Minimax model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        let valid_models = ["minimax-m3", "minimax-m2.7", "minimax-m2.7-highspeed"];
        if !valid_models.contains(&request.model.as_str()) {
            return Err(GatewayError::BadRequest(format!(
                "unknown Minimax model: {}",
                request.model
            )));
        }

        let has_attachments = request.messages.iter().any(|message| {
            !message.content.image_urls().is_empty() || !message.content.file_urls().is_empty()
        });
        if has_attachments && !self.supports_attachments() {
            return Err(GatewayError::BadRequest(format!(
                "model '{}' does not support file or image attachments",
                request.model
            )));
        }

        // The agent.minimax.io chat `message` backend has no native web-search
        // channel. Web search and deep-research are separate Agent skills
        // (server tools) that run outside the streaming chat payload this
        // provider sends, so the `search` toggle cannot be honoured here.
        // Fail closed instead of silently ignoring the flag.
        if request.search.unwrap_or(false) {
            return Err(GatewayError::BadRequest(format!(
                "model '{}' does not expose a web-search channel on the Minimax chat endpoint",
                request.model
            )));
        }

        // M2.7 reasoning is forced on and cannot be disabled (see
        // docs/MINIMAX_ARCHON_CONSTANTS.txt: m2.7 "thinking forced_on";
        // MiniMax platform docs: "For M2.x models, thinking cannot be
        // disabled"). The `thinking: false` toggle cannot be honoured, so
        // fail closed instead of silently ignoring it. M3 thinking is
        // switchable and is wired to the archon `model.variant` field.
        if request.thinking == Some(false) && request.model != "minimax-m3" {
            return Err(GatewayError::BadRequest(format!(
                "model '{}' always reasons: thinking is forced on and cannot be disabled",
                request.model
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimax_provider_exposes_expected_config() {
        let p = MinimaxProvider::new();
        assert_eq!(p.name(), "minimax");
        assert_eq!(p.url(), "https://agent.minimax.io");
        assert_eq!(p.models().len(), 3);
        assert!(p.models().iter().any(|m| m.id == "minimax-m3"));
        assert!(p.models().iter().any(|m| m.id == "minimax-m2.7"));
        assert!(p.models()
            .iter()
            .any(|m| m.id == "minimax-m2.7-highspeed"));

    }

    fn request(model: &str, search: Option<bool>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![],
            stream: false,
            session_url: None,
            thinking: None,
            search,
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
        }
    }

    fn request_with_thinking(model: &str, thinking: Option<bool>) -> ChatCompletionRequest {
        let mut r = request(model, None);
        r.thinking = thinking;
        r
    }

    #[test]
    fn minimax_accepts_chat_without_search() {
        let p = MinimaxProvider::new();
        assert!(p.validate_request(&request("minimax-m3", None)).is_ok());
        assert!(p
            .validate_request(&request("minimax-m2.7-highspeed", None))
            .is_ok());
    }

    #[test]
    fn minimax_rejects_search_flag_fail_closed() {
        let p = MinimaxProvider::new();
        let err = p
            .validate_request(&request("minimax-m3", Some(true)))
            .unwrap_err();
        assert!(err.to_string().contains("web-search"));
    }

    #[test]
    fn minimax_rejects_thinking_false_on_m27_fail_closed() {
        let p = MinimaxProvider::new();
        for model in ["minimax-m2.7", "minimax-m2.7-highspeed"] {
            let err = p
                .validate_request(&request_with_thinking(model, Some(false)))
                .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("always reasons"), "got: {msg}");
        }
        // Thinking on / unset is fine for the forced-on models.
        assert!(p
            .validate_request(&request_with_thinking("minimax-m2.7", Some(true)))
            .is_ok());
        assert!(p
            .validate_request(&request_with_thinking("minimax-m2.7", None))
            .is_ok());
    }

    #[test]
    fn minimax_accepts_thinking_toggle_on_m3() {
        let p = MinimaxProvider::new();
        // M3 thinking is switchable: both on and off validate.
        assert!(p
            .validate_request(&request_with_thinking("minimax-m3", Some(true)))
            .is_ok());
        assert!(p
            .validate_request(&request_with_thinking("minimax-m3", Some(false)))
            .is_ok());
    }

    #[test]
    fn quota_exhaustion_detects_upstream_markers() {
        let positives = [
            "Minimax returned finish_reason: error — 42212:Token Plan usage limit reached: Upgrade your Token Plan or purchase Credits for more usage. (2056)",
            "usage limit exceeded, 5-hour usage limit reached for Token Plan Starter (0/0 used) (2056)",
            "Token Plan usage limit reached",
        ];
        for msg in positives {
            assert!(
                is_minimax_quota_exhaustion(msg),
                "expected quota detection for: {msg}"
            );
        }
    }

    #[test]
    fn quota_exhaustion_ignores_unrelated_errors() {
        let negatives = [
            "Minimax returned finish_reason: error — server is busy, please retry later",
            "Minimax stream read error: IncompleteRead(201 bytes read)",
            "failed to parse session response: missing 'session_id'",
            "Minimax SSE request returned 401: unauthorized",
            "",
        ];
        for msg in negatives {
            assert!(
                !is_minimax_quota_exhaustion(msg),
                "expected no quota detection for: {msg}"
            );
        }
    }

    #[test]
    fn augment_quota_message_preserves_upstream_text_and_adds_remediation() {
        let msg = "Minimax returned finish_reason: error — 42212:Token Plan usage limit reached. (2056)";
        let augmented = augment_quota_message(msg);
        assert!(augmented.starts_with(msg), "upstream text must be preserved");
        assert!(augmented.contains("not retry-able"));
        assert!(augmented.contains("agent.minimax.io"));
    }

    #[tokio::test]
    async fn quota_block_fails_fast_and_holds_the_cached_message() {
        let p = MinimaxProvider::new();
        // No block initially.
        assert!(p.quota_blocked_message().await.is_none());

        // Setting a non-quota error must not create a block.
        p.set_quota_block_if_exhausted(&GatewayError::Provider(
            "server is busy, please retry later".into(),
        ))
        .await;
        assert!(p.quota_blocked_message().await.is_none());

        // A quota error creates a block whose message includes remediation.
        p.set_quota_block_if_exhausted(&GatewayError::Provider(
            "42212:Token Plan usage limit reached. (2056)".into(),
        ))
        .await;
        let msg = p.quota_blocked_message().await;
        let msg = msg.expect("quota block should be set");
        assert!(msg.contains("not retry-able"));
        assert!(msg.contains("agent.minimax.io"));
    }
}
