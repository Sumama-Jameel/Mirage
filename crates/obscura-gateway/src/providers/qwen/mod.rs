use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use futures::stream::BoxStream;
use futures::StreamExt;

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model};
use crate::providers::session_guard::SessionGuardStream;
use crate::providers::{ChatMode, DoneSignal, Provider};
use crate::session::SessionManager;
use crate::state::AppState;

use direct::DirectClient;
use state::QwenSessionStore;

mod auth;
mod direct;
mod state;
mod upload;

#[derive(Clone)]
pub struct QwenProvider {
    store: QwenSessionStore,
}

impl QwenProvider {
    pub fn new() -> Self {
        Self {
            store: QwenSessionStore::new(),
        }
    }

    /// Create a provider with optional disk-persisted sessions.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            store: QwenSessionStore::with_data_dir(data_dir),
        }
    }
}

impl Default for QwenProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for QwenProvider {
    fn name(&self) -> &'static str {
        "qwen"
    }

    fn url(&self) -> &'static str {
        "https://chat.qwen.ai"
    }

    fn models(&self) -> Vec<Model> {
        vec![
            Model {
                id: "qwen-auto".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "alibaba".to_string(),
            },
            Model {
                id: "qwen-plus".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "alibaba".to_string(),
            },
            Model {
                id: "qwen-max".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "alibaba".to_string(),
            },
            Model {
                id: "qwen-flash".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "alibaba".to_string(),
            },
            Model {
                id: "qwen-coder".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "alibaba".to_string(),
            },
            Model {
                id: "qwen-vl".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "alibaba".to_string(),
            },
            // Note: qwen-research is available through Alibaba Cloud API (DashScope) but not through
            // the free chat.qwen.ai endpoint used here. Removed until Deep Research API is added.
        ]
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
        let this = self.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            let session = sessions.acquire().await?;
            let client = match DirectClient::new(
                session.clone(),
                &sessions,
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
            let _ = sessions.release(session.id.clone(), false).await;
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
        let this = self.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            let session = sessions.acquire().await?;
            let client = match DirectClient::new(
                session.clone(),
                &sessions,
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
            let session_id = session.id.clone();
            let sessions_for_stream = sessions.clone();
            let stream = client.chat_stream(request, &session_id).await?;
            let guarded = SessionGuardStream::new(stream, sessions_for_stream, session_id);
            Ok(guarded.boxed())
        })
    }

    fn input_selectors(&self) -> &'static [&'static str] {
        &["textarea", "[contenteditable='true']"]
    }

    fn submit_selectors(&self) -> &'static [&'static str] {
        &["button[type='submit']", "[data-testid='send-button']"]
    }

    fn response_selector(&self) -> &'static str {
        "[data-testid='assistant-message']"
    }

    fn thinking_selector(&self) -> Option<&'static str> {
        None
    }

    fn done_signal(&self) -> DoneSignal {
        DoneSignal::TextStable(std::time::Duration::from_millis(1500))
    }

    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // Qwen web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Qwen model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        let valid_models = [
            "qwen-plus",
            "qwen-max",
            "qwen-flash",
            "qwen-coder",
            "qwen-vl",
            "qwen-auto",
        ];
        if !valid_models.contains(&request.model.as_str()) {
            return Err(GatewayError::BadRequest(format!(
                "unknown Qwen model: {}",
                request.model
            )));
        }

        let has_attachments = request.messages.iter().any(|message| {
            !message.content.image_urls().is_empty() || !message.content.file_urls().is_empty()
        });
        if has_attachments && !self.supports_attachments() {
            return Err(GatewayError::BadRequest(format!(
                "model '{}' does not support file or image attachments through its web UI yet",
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
    fn qwen_provider_exposes_expected_config() {
        let p = QwenProvider::new();
        assert_eq!(p.name(), "qwen");
        assert_eq!(p.url(), "https://chat.qwen.ai");
        assert_eq!(p.models().len(), 6);
        assert!(p.models().iter().any(|m| m.id == "qwen-plus"));
        assert!(p.models().iter().any(|m| m.id == "qwen-max"));
        assert!(p.models().iter().any(|m| m.id == "qwen-flash"));
        assert!(p.models().iter().any(|m| m.id == "qwen-coder"));
        assert!(p.models().iter().any(|m| m.id == "qwen-vl"));
        assert!(matches!(p.chat_mode(), ChatMode::Direct));
    }
}
