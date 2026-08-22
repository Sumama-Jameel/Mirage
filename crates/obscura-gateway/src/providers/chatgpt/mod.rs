use std::future::Future;
use std::pin::Pin;

use futures::stream::BoxStream;
use futures::StreamExt;

use std::path::PathBuf;

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
pub struct ChatGPTProvider {
    store: SessionStore,
}

impl ChatGPTProvider {
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

impl Default for ChatGPTProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for ChatGPTProvider {
    fn name(&self) -> &'static str {
        "chatgpt"
    }

    fn url(&self) -> &'static str {
        "https://chatgpt.com"
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

            let mut client = match direct::ChatGptDirectClient::new(
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

            let mut client = match direct::ChatGptDirectClient::new(
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
        // ChatGPT web API supports native JSON mode on vision-capable models.
        // Verified: gpt-4o and gpt-4o-mini are confirmed to support json_object via public API.
        // These models are likely exposed through the web UI /backend-api/conversation endpoint.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                // Only gpt-4o and gpt-4o-mini have confirmed json_object support.
                // o-series models (o1, o1-mini, o1-pro, o3-mini) do not support JSON mode.
                let supports_json = matches!(
                    request.model.as_str(),
                    "gpt-4o" | "gpt-4o-mini"
                );
                if !supports_json {
                    return Err(GatewayError::BadRequest(format!(
                        "ChatGPT model '{}' does not support response_format \"json_object\" (use gpt-4o or gpt-4o-mini)",
                        request.model
                    )));
                }
            }
        }

        // Thinking is supported on o-series models via a PATCH to the
        // settings endpoint. All other models reject it.
        if request.thinking == Some(true) {
            let thinking_capable = ["o1", "o1-mini", "o1-pro", "o3-mini"];
            if !thinking_capable.contains(&request.model.as_str()) {
                return Err(GatewayError::BadRequest(format!(
                    "ChatGPT model '{}' does not support thinking (use o1, o1-mini, o1-pro, or o3-mini)",
                    request.model
                )));
            }
        }

        // Web search is supported on all models via `force_search`,
        // except the legacy free tier.
        if request.search == Some(true) && request.model == "chatgpt-auto" {
            return Err(GatewayError::BadRequest(format!(
                "ChatGPT model 'chatgpt-auto' does not support web search (use gpt-4o, gpt-4o-mini, or another newer model)"
            )));
        }

        // The legacy free-tier model (chatgpt-auto → text-davinci-002-render-sha)
        // does not accept image attachments. All other registered models support
        // multimodal input.
        let supports_images = request.model != "chatgpt-auto";
        let has_images = request.messages.iter().any(|m| !m.content.image_urls().is_empty());
        if has_images && !supports_images {
            return Err(GatewayError::BadRequest(format!(
                "ChatGPT model '{}' does not support image attachments (use gpt-4o, gpt-4o-mini, or another vision-capable model)",
                request.model
            )));
        }

        Ok(())
    }
}

