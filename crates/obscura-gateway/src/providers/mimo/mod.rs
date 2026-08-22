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
use state::MiMoSessionStore;

mod direct;
mod state;
mod upload;

#[derive(Clone)]
pub struct MiMoProvider {
    store: MiMoSessionStore,
}

impl MiMoProvider {
    pub fn new() -> Self {
        Self {
            store: MiMoSessionStore::new(),
        }
    }

    /// Create a provider with optional disk-persisted sessions.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            store: MiMoSessionStore::with_data_dir(data_dir),
        }
    }
}

impl Default for MiMoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MiMoProvider {
    fn name(&self) -> &'static str {
        "mimo"
    }

    fn url(&self) -> &'static str {
        "https://aistudio.xiaomimimo.com"
    }

    fn models(&self) -> Vec<Model> {
        vec![
            Model {
                id: "mimo-v2.5-pro".to_string(),
                object: "model".to_string(),
                created: 1_767_239_114,
                owned_by: "xiaomi".to_string(),
            },
            Model {
                id: "mimo-v2.5".to_string(),
                object: "model".to_string(),
                created: 1_767_239_114,
                owned_by: "xiaomi".to_string(),
            },
            Model {
                id: "mimo-v2-flash".to_string(),
                object: "model".to_string(),
                created: 1_767_239_114,
                owned_by: "xiaomi".to_string(),
            },
            Model {
                id: "mimo-v2-pro".to_string(),
                object: "model".to_string(),
                created: 1_767_239_114,
                owned_by: "xiaomi".to_string(),
            },
            Model {
                id: "mimo-v2-omni".to_string(),
                object: "model".to_string(),
                created: 1_767_239_114,
                owned_by: "xiaomi".to_string(),
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

                let stream = match client.chat_stream(request.clone(), &session_id).await {
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
                GatewayError::Provider("MiMo: failed to acquire session".to_string())
            }))
        })
    }






    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // Mimo web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Mimo model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        let valid_models = [
            "mimo-v2.5-pro",
            "mimo-v2.5",
            "mimo-v2-flash",
            "mimo-v2-pro",
            "mimo-v2-omni",
        ];
        if !valid_models.contains(&request.model.as_str()) {
            return Err(GatewayError::BadRequest(format!(
                "unknown MiMo model: {}",
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

        // The MiMo web chat endpoint has no native function-calling channel:
        // the request payload carries only `enableThinking`, `webSearchStatus`
        // and the text `query` plus `multiMedias` (verified live against the
        // endpoint and the open-source mimo-chat-openai wrapper, which exposes
        // web_search as the only tool type). Per GOAL.md, when the root API
        // provably lacks the channel we inject the function definitions into
        // the prompt and parse `<tool_call>` XML markers out of the reply
        // (same fallback as DeepSeek/Gemini).

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mimo_provider_exposes_expected_config() {
        let p = MiMoProvider::new();
        assert_eq!(p.name(), "mimo");
        assert_eq!(p.url(), "https://aistudio.xiaomimimo.com");
        assert_eq!(p.models().len(), 5);
        assert!(p.models().iter().any(|m| m.id == "mimo-v2.5-pro"));
        assert!(p.models().iter().any(|m| m.id == "mimo-v2.5"));
        assert!(p.models().iter().any(|m| m.id == "mimo-v2-flash"));
        assert!(p.models().iter().any(|m| m.id == "mimo-v2-pro"));
        assert!(p.models().iter().any(|m| m.id == "mimo-v2-omni"));

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
    fn mimo_accepts_known_models() {
        let p = MiMoProvider::new();
        for model in ["mimo-v2.5-pro", "mimo-v2.5", "mimo-v2-flash", "mimo-v2-pro", "mimo-v2-omni"]
        {
            assert!(p.validate_request(&request(model)).is_ok(), "model: {model}");
        }
    }

    #[test]
    fn mimo_rejects_unknown_model() {
        let p = MiMoProvider::new();
        let err = p.validate_request(&request("mimo-unknown")).unwrap_err();
        assert!(err.to_string().contains("unknown MiMo model"));
    }

    #[test]
    fn mimo_accepts_function_tools_with_xml_fallback() {
        let p = MiMoProvider::new();
        let mut r = request("mimo-v2.5-pro");
        r.tools = Some(vec![crate::models::Tool {
            r#type: "function".to_string(),
            function: crate::models::FunctionDefinition {
                name: "get_weather".to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        }]);
        assert!(p.validate_request(&r).is_ok());
    }
}
