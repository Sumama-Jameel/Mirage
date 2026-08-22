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
use state::MetaAiSessionStore;

mod auth;
mod direct;
mod state;
mod templates;

/// Model ids this provider accepts, mapped to the DGW reasoning mode in
/// `direct::effective_mode` (with a per-request `thinking` override).
const MODEL_IDS: &[&str] = &["muse-spark", "muse-spark-thinking", "muse-spark-contemplating"];

/// Meta AI (meta.ai) provider.
///
/// Uses the Ecto-era DGW WebSocket transport with **native browser-session
/// auth**: session cookies from the user's browser profile plus the
/// page-injected `ecto1:` WebSocket token. There is no anonymous or temp-user
/// flow — missing auth fails closed with a descriptive `GatewayError::Auth`
/// (see `auth.rs` and `direct.rs`).
///
/// Model ids follow the meta.ai web app: `muse-spark` (instant, think_fast),
/// `muse-spark-thinking` and `muse-spark-contemplating` (think_hard). The old
/// anonymous `meta-ai` alias is retired.
#[derive(Clone)]
pub struct MetaAiProvider {
    store: MetaAiSessionStore,
}

impl MetaAiProvider {
    pub fn new() -> Self {
        Self {
            store: MetaAiSessionStore::new(),
        }
    }

    /// Create a provider with optional disk-persisted conversation state.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            store: MetaAiSessionStore::with_data_dir(data_dir),
        }
    }
}

impl Default for MetaAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MetaAiProvider {
    fn name(&self) -> &'static str {
        "metaai"
    }

    fn url(&self) -> &'static str {
        "https://www.meta.ai"
    }

    fn models(&self) -> Vec<Model> {
        MODEL_IDS
            .iter()
            .map(|id| Model {
                id: id.to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "meta".to_string(),
            })
            .collect()
    }


    /// Images are uploaded to the rupload endpoint and embedded in the DGW
    /// prompt proto as an attachment block.
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
                &request.model,
                this.store.clone(),
            )
            .await
            {
                Ok(client) => client,
                Err(e) => {
                    let dirty = matches!(e, GatewayError::Auth(_));
                    let _ = sessions.release(session.id, dirty).await;
                    return Err(e);
                }
            };
            let result = client.chat(request).await;
            let dirty = matches!(result, Err(GatewayError::Auth(_)));
            let _ = sessions.release(session.id.clone(), dirty).await;
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
                &request.model,
                this.store.clone(),
            )
            .await
            {
                Ok(client) => client,
                Err(e) => {
                    let dirty = matches!(e, GatewayError::Auth(_));
                    let _ = sessions.release(session.id, dirty).await;
                    return Err(e);
                }
            };
            let session_id = session.id.clone();
            let sessions_for_stream = sessions.clone();
            let stream = match client.chat_stream(request).await {
                Ok(stream) => stream,
                Err(e) => {
                    let dirty = matches!(e, GatewayError::Auth(_));
                    let _ = sessions.release(session.id, dirty).await;
                    return Err(e);
                }
            };
            let guarded = SessionGuardStream::new(stream, sessions_for_stream, session_id);
            Ok(guarded.boxed())
        })
    }






    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // Meta AI web API support for JSON mode is unverified.
        // Keep JSON mode blocked until live testing confirms support.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                return Err(GatewayError::BadRequest(format!(
                    "Meta AI model '{}' does not support response_format \"json_object\" (web API support unverified)",
                    request.model
                )));
            }
        }

        if !MODEL_IDS.contains(&request.model.as_str()) {
            return Err(GatewayError::BadRequest(format!(
                "unknown Meta AI model: {}. Supported models: {}",
                request.model,
                MODEL_IDS.join(", ")
            )));
        }

        // The DGW endpoint has no native web-search channel; fail closed
        if request.search.unwrap_or(false) {
            return Err(GatewayError::BadRequest(
                "muse-spark does not expose a web-search channel on the DGW endpoint".to_string(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(model: &str, search: Option<bool>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![],
            stream: false,
            session_url: None,
            thinking: None,
            search,
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
    fn metaai_provider_exposes_expected_config() {
        let p = MetaAiProvider::new();
        assert_eq!(p.name(), "metaai");
        assert_eq!(p.url(), "https://www.meta.ai");
        assert_eq!(p.models().len(), 3);
        assert!(p.models().iter().any(|m| m.id == "muse-spark"));
        assert!(p.models().iter().any(|m| m.id == "muse-spark-thinking"));
        assert!(p.models().iter().any(|m| m.id == "muse-spark-contemplating"));
        assert!(p.supports_attachments());

    }

    #[test]
    fn validate_rejects_unknown_models() {
        let p = MetaAiProvider::new();
        assert!(p.validate_request(&request("meta-ai", None)).is_err());
        assert!(p.validate_request(&request("muse-spark-4", None)).is_err());
    }

    #[test]
    fn validate_accepts_named_models() {
        let p = MetaAiProvider::new();
        assert!(p.validate_request(&request("muse-spark", None)).is_ok());
        assert!(p
            .validate_request(&request("muse-spark-thinking", None))
            .is_ok());
        assert!(p
            .validate_request(&request("muse-spark-contemplating", None))
            .is_ok());
    }

    #[test]
    fn validate_rejects_search_flag() {
        let p = MetaAiProvider::new();
        assert!(p.validate_request(&request("muse-spark", Some(true))).is_err());
    }

    #[test]
    fn validate_accepts_attachments() {
        let p = MetaAiProvider::new();
        let mut req = request("muse-spark", None);
        req.messages = vec![crate::models::ChatMessage {
            role: "user".to_string(),
            content: crate::models::ChatContent::Array(vec![crate::models::ContentPart::ImageUrl {
                image_url: crate::models::ImageUrl {
                    url: "data:image/png;base64,AAAA".to_string(),
                    detail: None,
                },
            }]),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        assert!(p.validate_request(&req).is_ok());
    }
}
