use std::path::PathBuf;

use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model};
use crate::providers::session_guard::SessionGuardStream;
use crate::providers::Provider;
use crate::session::SessionManager;
use crate::state::AppState;

use direct::DirectClient;
use state::MistralSessionStore;

mod direct;
mod state;
mod upload;

/// Model ids exposed by the Le Chat web app (verified in the bundle).
const MISTRAL_MODELS: [&str; 11] = [
    "mistral-large-latest",
    "mistral-large-2411",
    "mistral-large-2512",
    "mistral-medium-latest",
    "mistral-medium-2508",
    "mistral-medium-2508-lightspeed",
    "mistral-medium-3-5",
    "mistral-small-latest",
    "mistral-small-4",
    "mistral-small-2603",
    "mistral-deepresearch-2507",
];

#[derive(Clone)]
pub struct MistralProvider {
    store: MistralSessionStore,
}

impl MistralProvider {
    pub fn new() -> Self {
        Self {
            store: MistralSessionStore::new(),
        }
    }

    /// Create a provider with optional disk-persisted sessions.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            store: MistralSessionStore::with_data_dir(data_dir),
        }
    }
}

impl Default for MistralProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MistralProvider {
    /// Reject a model switch on an existing thread before spending a pool
    /// session; appending with a different model silently starts unexpected
    /// behavior on the server side. Applies to both streaming and
    /// non-streaming paths.
    async fn reject_model_switch(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        let Some(url) = request.session_url.as_deref() else {
            return Ok(());
        };
        let Some(token) = url
            .strip_prefix("mistral://session/")
            .and_then(|s| s.split('?').next())
        else {
            return Ok(());
        };
        if let Some(state) = self.store.get(token).await {
            if state.model != request.model {
                return Err(GatewayError::BadRequest(format!(
                    "cannot continue a Mistral chat (model '{}') with model '{}'; \
                     start a new conversation to switch models",
                    state.model, request.model
                )));
            }
        }
        Ok(())
    }
}

impl Provider for MistralProvider {
    fn name(&self) -> &'static str {
        "mistral"
    }

    fn url(&self) -> &'static str {
        "https://chat.mistral.ai"
    }

    fn models(&self) -> Vec<Model> {
        MISTRAL_MODELS
            .iter()
            .map(|id| Model {
                id: (*id).to_string(),
                object: "model".to_string(),
                created: 1_767_000_000,
                owned_by: "mistral".to_string(),
            })
            .collect()
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
            this.reject_model_switch(&request).await?;

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
        let store = this.store.clone();
        Box::pin(async move {
            let max_attempts = 2;
            let mut last_error: Option<GatewayError> = None;

            if let Err(e) = this.reject_model_switch(&request).await {
                return Err(e);
            }

            for _attempt in 1..=max_attempts {
                let session = match sessions.acquire().await {
                    Ok(s) => s,
                    Err(e) => {
                        last_error = Some(e);
                        continue;
                    }
                };

                let client = match DirectClient::new(session.clone(), &request.model, store.clone())
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

                let stream = match client.chat_stream(request.clone()).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = sessions.release(session_id, false).await;
                        last_error = Some(e);
                        continue;
                    }
                };

                let (tx, rx) = mpsc::unbounded_channel();

                tokio::spawn(async move {
                    let mut stream = stream;
                    while let Some(chunk) = stream.next().await {
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
                GatewayError::Provider("Mistral: failed to acquire session".to_string())
            }))
        })
    }






    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // Mistral web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Mistral model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        if !MISTRAL_MODELS.contains(&request.model.as_str()) {
            return Err(GatewayError::BadRequest(format!(
                "unknown Mistral model: {}",
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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mistral_provider_exposes_expected_config() {
        let p = MistralProvider::new();
        assert_eq!(p.name(), "mistral");
        assert_eq!(p.url(), "https://chat.mistral.ai");
        assert_eq!(p.models().len(), MISTRAL_MODELS.len());
        assert!(p.models().iter().any(|m| m.id == "mistral-large-2512"));
        assert!(p
            .models()
            .iter()
            .any(|m| m.id == "mistral-deepresearch-2507"));

    }

    fn request(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
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
        }
    }

    #[test]
    fn mistral_accepts_known_models() {
        let p = MistralProvider::new();
        for model in MISTRAL_MODELS {
            assert!(p.validate_request(&request(model)).is_ok(), "model: {model}");
        }
    }

    #[test]
    fn mistral_rejects_unknown_model() {
        let p = MistralProvider::new();
        let err = p.validate_request(&request("mistral-unknown")).unwrap_err();
        assert!(err.to_string().contains("unknown Mistral model"));
    }
}
