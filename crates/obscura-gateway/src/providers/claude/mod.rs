use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use futures::stream::BoxStream;
use futures::StreamExt;

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model};
use crate::providers::session_guard::SessionGuardStream;
use crate::providers::Provider;
use crate::session::SessionManager;
use crate::state::AppState;

use self::state::SessionStore;

mod auth;
mod direct;
mod models;
mod rpc;
mod state;
mod upload;

pub use models::to_public_models;

#[derive(Clone)]
pub struct ClaudeProvider {
    store: SessionStore,
}

impl ClaudeProvider {
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

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn url(&self) -> &'static str {
        "https://claude.ai"
    }

    fn models(&self) -> Vec<Model> {
        to_public_models()
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

            let mut client = match direct::ClaudeDirectClient::new(
                &session, &model_id, store,
            ).await {
                Ok(c) => c,
                Err(e) => {
                    let _ = sessions.release(session_clone.id, false).await;
                    return Err(e);
                }
            };

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

            let mut client = match direct::ClaudeDirectClient::new(
                &session, &model_id, store,
            ).await {
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





    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // Claude web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Claude model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        let model_def = models::resolve_model(&request.model).ok_or_else(|| {
            GatewayError::BadRequest(format!("unknown Claude model: {}", request.model))
        })?;

        if request.thinking == Some(true) && !model_def.supports_thinking {
            return Err(GatewayError::BadRequest(format!(
                "Claude model '{}' does not support thinking (use a current-gen model like claude-sonnet-5)",
                request.model
            )));
        }

        if request.search == Some(true) && !model_def.supports_search {
            return Err(GatewayError::BadRequest(format!(
                "Claude model '{}' does not support web search",
                request.model
            )));
        }

        let has_images = request.messages.iter().any(|m| !m.content.image_urls().is_empty());
        if has_images && !model_def.supports_vision {
            return Err(GatewayError::BadRequest(format!(
                "Claude model '{}' does not support image inputs",
                request.model
            )));
        }

        let has_files = request.messages.iter().any(|m| !m.content.file_urls().is_empty());
        if has_files && !model_def.supports_vision {
            return Err(GatewayError::BadRequest(format!(
                "Claude model '{}' does not support file attachments",
                request.model
            )));
        }

        if request.tools.is_some() && !model_def.supports_tools {
            return Err(GatewayError::BadRequest(format!(
                "Claude model '{}' does not support function calling",
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
    fn claude_provider_exposes_expected_config() {
        let p = ClaudeProvider::new();
        assert_eq!(p.name(), "claude");
        assert_eq!(p.url(), "https://claude.ai");
        assert!(p.models().iter().any(|m| m.id == "claude-sonnet-5"));

    }

    #[test]
    fn models_include_all_variants() {
        let p = ClaudeProvider::new();
        let models = p.models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"claude-sonnet-5"));
        assert!(ids.contains(&"claude-fable-5"));
        assert!(ids.contains(&"claude-opus-5"));
        assert!(ids.contains(&"claude-opus-4-8"));
        assert!(ids.contains(&"claude-haiku-4-5"));
        assert!(ids.contains(&"claude-sonnet-4-6"));
        assert!(ids.contains(&"claude-opus-4-7"));
        assert!(ids.contains(&"claude-sonnet-4-5"));
        assert!(ids.contains(&"claude-sonnet-4-5-20250929"));
        assert!(ids.contains(&"claude-3-opus-20240229"));
        assert_eq!(ids.len(), 11);
    }
}
