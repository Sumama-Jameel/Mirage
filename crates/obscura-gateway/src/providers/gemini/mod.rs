use std::future::Future;
use std::pin::Pin;

use futures::stream::BoxStream;
use futures::StreamExt;

use std::path::PathBuf;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model,
};
use crate::providers::session_guard::SessionGuardStream;
use crate::providers::{ChatMode, DoneSignal, Provider};
use self::state::SessionStore;
use crate::session::SessionManager;
use crate::state::AppState;

mod auth;
mod direct;
mod models;
mod rpc;
mod state;
mod upload;

pub use models::to_public_models;

/// Gemini web-UI provider.
///
/// Uses Google's internal StreamGenerate RPC endpoint to obtain chat
/// completions. Authentication is via Google session cookies (imported
/// from the user's browser) and the SNlM0e CSRF token extracted from
/// the gemini.google.com page.
#[derive(Clone)]
pub struct GeminiProvider {
    store: SessionStore,
}

impl GeminiProvider {
    pub fn new() -> Self {
        Self {
            store: SessionStore::new(),
        }
    }

    /// Create a provider with optional disk-persisted sessions.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            store: SessionStore::with_data_dir(data_dir),
        }
    }
}

impl Default for GeminiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn url(&self) -> &'static str {
        "https://gemini.google.com"
    }

    fn models(&self) -> Vec<Model> {
        to_public_models()
    }

    fn chat_mode(&self) -> ChatMode {
        ChatMode::Direct
    }

    fn supports_attachments(&self) -> bool {
        true
    }

    fn chat(
        &self,
        sessions: &SessionManager,
        _state: &AppState,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>> {
        let sessions = sessions.clone();
        let model_id = request.model.clone();
        let store = self.store.clone();
        let request_session_url = request.session_url.clone();
        Box::pin(async move {
            let session = sessions.acquire().await?;
            let session_clone = session.clone();

            // Resolve conversation state from session_url via SessionStore
            // (handled inside GeminiDirectClient).
            let mut client = match direct::GeminiDirectClient::new(
                &session,
                &model_id,
                sessions.clone(),
                None,
                store,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = sessions.release(session_clone.id, false).await;
                    return Err(e);
                }
            };

            // Restore conversation from session_url if provided.
            // (done inside client.chat() via stored session).
            // It creates a new request without the prev_conversation,
            // but the client.chat() will look it up from the store.

            let mut request_with_url = request;
            request_with_url.session_url = request_session_url;
            let result = client.chat(request_with_url).await;
            let _ = sessions.release(session_clone.id, false).await;
            result
        })
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
        let model_id = request.model.clone();
        let store = self.store.clone();
        let request_session_url = request.session_url.clone();
        Box::pin(async move {
            let session = sessions.acquire().await?;
            let session_id = session.id.clone();
            let sessions_for_stream = sessions.clone();

            let mut client = match direct::GeminiDirectClient::new(
                &session,
                &model_id,
                sessions.clone(),
                None,
                store,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = sessions.release(session_id, false).await;
                    return Err(e);
                }
            };

            let mut request_with_url = request;
            request_with_url.session_url = request_session_url;
            let stream = client.chat_stream(request_with_url).await?;
            let guarded = SessionGuardStream::new(stream, sessions_for_stream, session_id);
            Ok(guarded.boxed())
        })
    }

    fn input_selectors(&self) -> &'static [&'static str] {
        &["textarea", "[contenteditable='true']"]
    }

    fn submit_selectors(&self) -> &'static [&'static str] {
        &["button[type='submit']", "button.send-button"]
    }

    fn response_selector(&self) -> &'static str {
        ".response, .conversation-turn"
    }

    fn thinking_selector(&self) -> Option<&'static str> {
        Some(".thinking, .reasoning-panel")
    }

    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // Gemini web API support for JSON mode is unclear.
        // StreamGenerate RPC explicitly ignores sampling parameters per BROKEN_FEATURES.md.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Gemini model '{}' does not support response_format \"json_object\" (web API support unclear)",
                    request.model
                )));
            }
        }

        let model_def = models::resolve_model(&request.model).ok_or_else(|| {
            GatewayError::BadRequest(format!("unknown Gemini model: {}", request.model))
        })?;

        if request.thinking == Some(true) && !model_def.supports_thinking {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support thinking (use gemini-3.5-flash or gemini-3.1-pro)",
                request.model
            )));
        }

        if request.search == Some(true) && !model_def.supports_search {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support web search",
                request.model
            )));
        }

        let has_images = request.messages.iter().any(|m| !m.content.image_urls().is_empty());
        if has_images && !model_def.supports_vision {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support image inputs",
                request.model
            )));
        }

        let has_files = request.messages.iter().any(|m| !m.content.file_urls().is_empty());
        if has_files && !model_def.supports_vision {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support file attachments",
                request.model
            )));
        }

        if request.tools.is_some() && !model_def.supports_tools {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support function calling",
                request.model
            )));
        }

        // Generation parameters are not supported in Gemini's internal web API
        // (StreamGenerate endpoint). They are only available through the official
        // Google AI API at generativelanguage.googleapis.com (requires API key).
        if request.temperature.is_some() {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support temperature (use the official Google AI API with an API key for generation parameter control)",
                request.model
            )));
        }
        if request.max_tokens.is_some() {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support max_tokens",
                request.model
            )));
        }
        if request.top_p.is_some() {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support top_p",
                request.model
            )));
        }
        if request.stop.is_some() {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support stop sequences",
                request.model
            )));
        }
        if request.presence_penalty.is_some() {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support presence_penalty",
                request.model
            )));
        }
        if request.frequency_penalty.is_some() {
            return Err(GatewayError::BadRequest(format!(
                "Gemini model '{}' does not support frequency_penalty",
                request.model
            )));
        }

        Ok(())
    }

    fn done_signal(&self) -> DoneSignal {
        DoneSignal::TextStable(std::time::Duration::from_millis(2000))
    }
}

/// A stream wrapper that releases the browser session when the consumer is
/// done, even if the underlying stream ends with an error.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::gemini::models::GEMINI_MODELS;

    #[test]
    fn gemini_provider_exposes_expected_config() {
        let p = GeminiProvider::new();
        assert_eq!(p.name(), "gemini");
        assert_eq!(p.url(), "https://gemini.google.com");
        assert!(p.models().iter().any(|m| m.id == "gemini-3.5-flash"));
        assert!(p.models().iter().any(|m| m.id == "gemini-3.1-pro"));
        assert!(matches!(p.chat_mode(), ChatMode::Direct));
    }

    #[test]
    fn models_include_all_variants() {
        let p = GeminiProvider::new();
        let models = p.models();
        assert_eq!(models.len(), GEMINI_MODELS.len());
        for expected in GEMINI_MODELS {
            assert!(
                models.iter().any(|m| m.id == expected.id),
                "missing model: {}",
                expected.id
            );
        }
    }
}
