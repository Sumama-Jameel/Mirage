//! GLM provider for the free `chat.z.ai` web UI.
//!
//! The provider calls Z.AI's internal HTTP API directly from Rust, using the
//! warmed browser session only for authenticated cookies/localStorage. The
//! upstream surface is the captured wire protocol documented in
//! `docs/wire/glm-v2.md` (SSE `chat:completion` events, `/api/v1/files/`
//! uploads, `/api/v1/chats/new` creation).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use futures::stream::BoxStream;
use tracing::info;

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model};
use crate::providers::Provider;
use crate::session::SessionManager;
use crate::state::AppState;

use self::direct::{DirectError, GlmDirectClient};
use self::models::{resolve_model, GlmModelDef};
use self::state::SessionStore;

mod auth;
mod direct;
mod models;
mod rpc;
mod signature;
mod state;
mod upload;

pub use models::to_public_models;

// Re-export the web-app URL used by the direct path and tests.
pub(crate) use self::direct::CHAT_Z_AI_URL;

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

    /// Run a chat completion through the direct internal-API path. Recoverable
    /// failures surface as directed `DirectError`s; nothing falls back to a
    /// UI flow (removed in the UI-automation deletion).
    async fn run_direct(
        &self,
        sessions: &SessionManager,
        state: &AppState,
        session: &crate::session::SessionHandle,
        model: &GlmModelDef,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let client = GlmDirectClient::new(
            sessions,
            session,
            self.store.clone(),
            model,
            &state.config.glm.sign_secret,
            &state.config.glm.upstream_url,
            state.config.browser.profile_path.as_deref().map(std::path::Path::new),
            crate::providers::drift_snapshot::DriftSnapshots::with_data_dir(
                state.config.data_dir.clone(),
            ),
        )
        .await;

        match client {
            Ok(client) => match client.chat(request.clone()).await {
                Ok(response) => {
                    info!(model = %model.id, "GLM direct path succeeded");
                    Ok(response)
                }
                Err(DirectError::Fallback(reason)) => {
                    Err(GatewayError::Provider(format!("GLM direct path failed: {reason}")))
                }
                Err(DirectError::Fatal(e)) => Err(e),
            },
            Err(DirectError::Fallback(reason)) => {
                Err(GatewayError::Provider(format!("GLM direct client unavailable: {reason}")))
            }
            Err(DirectError::Fatal(e)) => Err(e),
        }
    }

    /// Run a streaming completion through the direct internal-API path.
    async fn run_stream_direct(
        &self,
        sessions: &SessionManager,
        state: &AppState,
        session: &crate::session::SessionHandle,
        model: &GlmModelDef,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let client = GlmDirectClient::new(
            sessions,
            session,
            self.store.clone(),
            model,
            &state.config.glm.sign_secret,
            &state.config.glm.upstream_url,
            state.config.browser.profile_path.as_deref().map(std::path::Path::new),
            crate::providers::drift_snapshot::DriftSnapshots::with_data_dir(
                state.config.data_dir.clone(),
            ),
        )
        .await;

        match client {
            Ok(client) => match client.chat_stream(request.clone()).await {
                Ok(stream) => {
                    info!(model = %model.id, "GLM direct streaming path succeeded");
                    Ok(stream)
                }
                Err(DirectError::Fallback(reason)) => Err(GatewayError::Provider(format!(
                    "GLM direct stream failed: {reason}"
                ))),
                Err(DirectError::Fatal(e)) => Err(e),
            },
            Err(DirectError::Fallback(reason)) => Err(GatewayError::Provider(format!(
                "GLM direct client unavailable: {reason}"
            ))),
            Err(DirectError::Fatal(e)) => Err(e),
        }
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

            let result = provider.run_direct(&sessions, &state, &session, &model, request).await;
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
                .run_stream_direct(&sessions, &state, &session, &model, request)
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
