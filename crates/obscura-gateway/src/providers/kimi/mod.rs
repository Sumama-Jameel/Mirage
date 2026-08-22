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
pub struct KimiProvider {
    store: SessionStore,
}

impl KimiProvider {
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

impl Default for KimiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for KimiProvider {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn url(&self) -> &'static str {
        "https://kimi.moonshot.cn"
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

            let mut client = match direct::KimiDirectClient::new(
                &session, &model_id, &sessions, store,
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

            let mut client = match direct::KimiDirectClient::new(
                &session, &model_id, &sessions, store,
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
        // Kimi web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Kimi model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        let model_def = crate::providers::kimi::models::resolve_model(&request.model)
            .ok_or_else(|| GatewayError::BadRequest(format!("unknown Kimi model: {}", request.model)))?;

        if request.thinking == Some(true) && !model_def.is_thinking {
            return Err(GatewayError::BadRequest(format!(
                "Kimi model '{}' does not support thinking",
                request.model
            )));
        }

        if request.search == Some(true) {
            if model_def.kimiplus_id != "kimi" {
                return Err(GatewayError::BadRequest(format!(
                    "Kimi model '{}' does not support web search (use kimi-search)",
                    request.model
                )));
            }
        }

        let has_images = request.messages.iter().any(|m| !m.content.image_urls().is_empty());
        if has_images && !model_def.supports_vision {
            return Err(GatewayError::BadRequest(format!(
                "Kimi model '{}' does not support image inputs",
                request.model
            )));
        }

        let has_files = request.messages.iter().any(|m| !m.content.file_urls().is_empty());
        if has_files && !model_def.supports_vision {
            return Err(GatewayError::BadRequest(format!(
                "Kimi model '{}' does not support file attachments",
                request.model
            )));
        }

        if request.tools.is_some() && !model_def.supports_tools {
            return Err(GatewayError::BadRequest(format!(
                "Kimi model '{}' does not support function calling",
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
    fn kimi_provider_exposes_expected_config() {
        let p = KimiProvider::new();
        assert_eq!(p.name(), "kimi");
        assert_eq!(p.url(), "https://kimi.moonshot.cn");
        assert!(p.models().iter().any(|m| m.id == "kimi-k2.7-code"));

    }

    #[test]
    fn models_include_all_variants() {
        let p = KimiProvider::new();
        let models = p.models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"kimi-k3"));
        assert!(ids.contains(&"kimi-k3-instant"));
        assert!(ids.contains(&"kimi-k3-swarm"));
        assert!(ids.contains(&"kimi-k2.7-code"));
        assert!(ids.contains(&"kimi-k2.7-code-highspeed"));
        assert!(ids.contains(&"kimi-k2.6"));
        assert!(ids.contains(&"kimi-k2.5"));
        assert!(ids.contains(&"kimi-search"));
        assert!(ids.contains(&"kimi-research"));
        assert_eq!(ids.len(), 9);
    }
}
