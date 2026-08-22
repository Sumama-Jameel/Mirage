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

use direct::DirectClient;
use state::GrokSessionStore;

mod auth;
mod direct;
mod state;
mod statsig;
mod upload;

#[derive(Clone)]
pub struct GrokProvider {
    store: GrokSessionStore,
}

impl GrokProvider {
    pub fn new() -> Self {
        Self {
            store: GrokSessionStore::new(),
        }
    }

    /// Create a provider with optional disk-persisted sessions.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            store: GrokSessionStore::with_data_dir(data_dir),
        }
    }
}

impl Default for GrokProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for GrokProvider {
    fn name(&self) -> &'static str {
        "grok"
    }

    fn url(&self) -> &'static str {
        "https://grok.com"
    }

    fn models(&self) -> Vec<Model> {
        vec![
            Model {
                id: "grok-auto".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "xai".to_string(),
            },
            Model {
                id: "grok-fast".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "xai".to_string(),
            },
            Model {
                id: "grok-expert".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "xai".to_string(),
            },
            Model {
                id: "grok-heavy".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "xai".to_string(),
            },
            Model {
                id: "grok-4.5".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "xai".to_string(),
            },
            Model {
                id: "grok-4.3".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "xai".to_string(),
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
    ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>> {
        let this = self.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            let session = sessions.acquire().await?;
            let client = match DirectClient::new(session.clone(), &request.model, this.store.clone())
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = sessions.release(session.id, false).await;
                    return Err(e);
                }
            };
            let result = client.chat(request).await;
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
            let client = match DirectClient::new(session.clone(), &request.model, this.store.clone())
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
            let stream = client.chat_stream(request).await?;
            let guarded = SessionGuardStream::new(stream, sessions_for_stream, session_id);
            Ok(guarded.boxed())
        })
    }






    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // Grok web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Grok model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        let valid_models = [
            "grok-auto",
            "grok-fast",
            "grok-expert",
            "grok-heavy",
            "grok-4.5",
            "grok-4.3",
        ];
        if !valid_models.contains(&request.model.as_str()) {
            return Err(GatewayError::BadRequest(format!(
                "unknown Grok model: {}",
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
    fn grok_provider_exposes_expected_config() {
        let p = GrokProvider::new();
        assert_eq!(p.name(), "grok");
        assert_eq!(p.url(), "https://grok.com");
        assert_eq!(p.models().len(), 6);
        assert!(p.models().iter().any(|m| m.id == "grok-auto"));
        assert!(p.models().iter().any(|m| m.id == "grok-fast"));
        assert!(p.models().iter().any(|m| m.id == "grok-expert"));
        assert!(p.models().iter().any(|m| m.id == "grok-heavy"));
        assert!(p.models().iter().any(|m| m.id == "grok-4.5"));
        assert!(p.models().iter().any(|m| m.id == "grok-4.3"));

    }
}
