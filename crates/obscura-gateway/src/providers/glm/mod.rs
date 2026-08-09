//! GLM provider for the free `chat.z.ai` web UI.
//!
//! The provider prefers Z.AI's internal HTTP API (like DeepSeek/Gemini/ChatGPT)
//! and falls back to driving the authenticated browser UI only when the direct
//! path cannot be used: missing token, signature failure, captcha challenge, or
//! attachment upload failure.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use tracing::{info, warn};

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model};
use crate::providers::session_guard::SessionGuardStream;
use crate::providers::{ChatMode, DoneSignal, Provider};
use crate::session::SessionManager;
use crate::state::AppState;

use self::direct::{DirectError, GlmDirectClient};
use self::models::{resolve_model, GlmModelDef};
use self::state::SessionStore;

mod auth;
mod captcha;
mod direct;
mod humanize;
mod models;
mod response;
mod rpc;
mod signature;
mod state;
mod ui;
mod upload;

pub use models::to_public_models;

// Re-export UI constants used by the direct path and tests.
pub(crate) use ui::{CHAT_Z_AI_URL, RESPONSE_SELECTOR, THINKING_SELECTOR};

/// Validate that a remote URL does not point to a private or loopback
/// network. Shared by the direct upload and UI attachment paths to prevent
/// SSRF attacks when resolving attachment URLs server-side.
pub(crate) fn validate_remote_url(url: &str) -> Result<(), GatewayError> {
    let parsed = url::Url::parse(url)
        .map_err(|_| GatewayError::BadRequest(format!("invalid URL: {url}")))?;
    let host = parsed
        .host()
        .ok_or_else(|| GatewayError::BadRequest(format!("no host in URL: {url}")))?;

    match host {
        url::Host::Domain(domain) => {
            if domain == "localhost" || domain == "127.0.0.1" {
                return Err(GatewayError::BadRequest("local addresses are blocked".to_string()));
            }
            Ok(())
        }
        url::Host::Ipv4(ip) => {
            if ip.is_loopback() || ip.is_private() || ip.is_link_local() {
                return Err(GatewayError::BadRequest(
                    "private IPv4 addresses are blocked".to_string(),
                ));
            }
            Ok(())
        }
        url::Host::Ipv6(ip) => {
            if ip.is_loopback() || ip.is_unicast_link_local() || ip.is_unique_local() {
                return Err(GatewayError::BadRequest(
                    "private IPv6 addresses are blocked".to_string(),
                ));
            }
            Ok(())
        }
    }
}

/// GLM provider. Cheaply cloneable.
#[derive(Clone)]
pub struct GlmProvider {
    store: SessionStore,
}

impl GlmProvider {
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

    /// Try the direct internal-API path. On a recoverable failure, fall back
    /// to the UI-automation path.
    async fn run_with_fallback(
        &self,
        sessions: &SessionManager,
        state: &AppState,
        session: &crate::session::SessionHandle,
        model: &GlmModelDef,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        if !state.config.glm.force_ui {
            let client = GlmDirectClient::new(
                sessions,
                session,
                self.store.clone(),
                model,
                &state.config.glm.sign_secret,
                &state.config.glm.upstream_url,
            )
            .await;

            match client {
                Ok(client) => match client.chat(request.clone()).await {
                    Ok(response) => {
                        info!(model = %model.id, "GLM direct path succeeded");
                        return Ok(response);
                    }
                    Err(DirectError::Fallback(reason)) => {
                        warn!(reason = %reason, "GLM direct path failed; falling back to UI");
                    }
                    Err(DirectError::Fatal(e)) => return Err(e),
                },
                Err(DirectError::Fallback(reason)) => {
                    warn!(reason = %reason, "GLM direct client unavailable; falling back to UI");
                }
                Err(DirectError::Fatal(e)) => return Err(e),
            }
        }

        ui::run_glm_chat(sessions, session, &self.store, model, request).await
    }

    /// Try the direct internal-API streaming path. On a recoverable failure,
    /// fall back to the UI-automation stream.
    async fn run_stream_with_fallback(
        &self,
        sessions: &SessionManager,
        state: &AppState,
        session: &crate::session::SessionHandle,
        model: &GlmModelDef,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        if !state.config.glm.force_ui {
            let client = GlmDirectClient::new(
                sessions,
                session,
                self.store.clone(),
                model,
                &state.config.glm.sign_secret,
                &state.config.glm.upstream_url,
            )
            .await;

            match client {
                Ok(client) => match client.chat_stream(request.clone()).await {
                    Ok(stream) => {
                        info!(model = %model.id, "GLM direct streaming path succeeded");
                        return Ok(stream);
                    }
                    Err(DirectError::Fallback(reason)) => {
                        warn!(reason = %reason, "GLM direct stream failed; falling back to UI");
                    }
                    Err(DirectError::Fatal(e)) => return Err(e),
                },
                Err(DirectError::Fallback(reason)) => {
                    warn!(reason = %reason, "GLM direct client unavailable; falling back to UI");
                }
                Err(DirectError::Fatal(e)) => return Err(e),
            }
        }

        let sessions_for_stream = sessions.clone();
        let session_id = session.id.clone();
        let stream = ui::run_glm_chat_stream(sessions, session, &self.store, model, request).await?;
        Ok(SessionGuardStream::new(stream, sessions_for_stream, session_id).boxed())
    }
}

impl Default for GlmProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for GlmProvider {
    fn name(&self) -> &'static str {
        "glm"
    }

    fn url(&self) -> &'static str {
        CHAT_Z_AI_URL
    }

    fn models(&self) -> Vec<Model> {
        to_public_models()
    }

    fn chat_mode(&self) -> ChatMode {
        ChatMode::Direct
    }

    fn chat(
        &self,
        sessions: &SessionManager,
        state: &AppState,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>> {
        let sessions = sessions.clone();
        let state = state.clone();
        let provider = self.clone();
        Box::pin(async move {
            let model = resolve_model(&request.model).ok_or_else(|| {
                GatewayError::BadRequest(format!("unknown GLM model: {}", request.model))
            })?;

            let session = sessions.acquire().await?;
            let session_clone = session.clone();

            let result = provider.run_with_fallback(&sessions, &state, &session, &model, request).await;
            let _ = sessions.release(session_clone.id, false).await;
            result
        })
    }

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
    > {
        let sessions = sessions.clone();
        let state = state.clone();
        let provider = self.clone();
        Box::pin(async move {
            let model = resolve_model(&request.model).ok_or_else(|| {
                GatewayError::BadRequest(format!("unknown GLM model: {}", request.model))
            })?;

            let session = sessions.acquire().await?;
            let session_id = session.id.clone();
            let sessions_for_release = sessions.clone();

            let result = provider
                .run_stream_with_fallback(&sessions, &state, &session, &model, request)
                .await;

            match result {
                Ok(stream) => Ok(stream),
                Err(e) => {
                    let _ = sessions_for_release.release(session_id, false).await;
                    Err(e)
                }
            }
        })
    }

    fn input_selectors(&self) -> &'static [&'static str] {
        &[
            "textarea[placeholder*='Ask' i]",
            "textarea[placeholder*='Message' i]",
            "textarea[placeholder*='message' i]",
            "[contenteditable='true'][role='textbox']",
            "textarea",
        ]
    }

    fn submit_selectors(&self) -> &'static [&'static str] {
        &[
            "button[aria-label*='send' i]",
            "button[type='submit']",
        ]
    }

    fn response_selector(&self) -> &'static str {
        RESPONSE_SELECTOR
    }

    fn thinking_selector(&self) -> Option<&'static str> {
        Some(THINKING_SELECTOR)
    }

    fn done_signal(&self) -> DoneSignal {
        DoneSignal::TextStable(Duration::from_millis(2000))
    }

    fn supports_attachments(&self) -> bool {
        true
    }

    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // GLM web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "GLM model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        let model = resolve_model(&request.model).ok_or_else(|| {
            GatewayError::BadRequest(format!("unknown GLM model: {}", request.model))
        })?;

        if request.tools.is_some() && !model.supports_tools {
            return Err(GatewayError::BadRequest(format!(
                "GLM model '{}' does not support tools",
                request.model
            )));
        }

        if request.thinking == Some(true) && !model.supports_thinking {
            return Err(GatewayError::BadRequest(format!(
                "GLM model '{}' does not support thinking",
                request.model
            )));
        }

        if request.search == Some(true) && !model.supports_search {
            return Err(GatewayError::BadRequest(format!(
                "GLM model '{}' does not support web search",
                request.model
            )));
        }

        for msg in &request.messages {
            if !msg.content.image_urls().is_empty() && !model.supports_vision {
                return Err(GatewayError::BadRequest(format!(
                    "GLM model '{}' does not support image inputs",
                    request.model
                )));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_provider_exposes_expected_config() {
        let p = GlmProvider::new();
        assert_eq!(p.name(), "glm");
        assert_eq!(p.url(), "https://chat.z.ai");
        assert!(p.models().iter().any(|m| m.id == "glm-5.2"));
        assert!(matches!(p.chat_mode(), ChatMode::Direct));
    }

    #[test]
    fn glm_provider_allows_named_models() {
        let p = GlmProvider::new();
        let models = p.models();
        for expected in ["glm-5.2", "glm-5.1", "glm-5-turbo", "glm-4.7"] {
            assert!(
                models.iter().any(|m| m.id == expected),
                "missing model: {}",
                expected
            );
        }
    }
}
