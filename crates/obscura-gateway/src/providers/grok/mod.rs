use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use futures::stream::BoxStream;
use futures::StreamExt;

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatContent, ChatMessage, Model, Usage};
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
pub mod statsig_harvest;
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
        state: &AppState,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>> {
        let this = self.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            this.store.load_token_pool_if_empty();

            let session = sessions.acquire().await?;
            // Browser-as-transport: navigate the session to grok.com and
            // make the API call from within the page. The app's own JS
            // interceptors add x-statsig-id, Baggage, and all other auth
            // headers automatically — no token extraction needed.
            sessions.navigate(&session.id, "https://grok.com").await?;
            tracing::info!("grok browser-transport: navigating to grok.com");
            for _ in 0..20 {
                sessions.pump_event_loop(&session.id, 2000).await.ok();
                let ready = sessions.execute_js(&session.id,
                    r#"document.readyState === 'complete' && document.querySelector('textarea, [contenteditable]') !== null"#
                ).await.unwrap_or(serde_json::Value::Null);
                if ready.as_str() == Some("true") {
                    tracing::info!("grok browser-transport: page ready");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }

            // Build payload using DirectClient's builder.
            let client = match DirectClient::new(session.clone(), &request.model, this.store.clone())
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = sessions.release(session.id, false).await;
                    return Err(e);
                }
            };
            let conversation_id = uuid::Uuid::new_v4().to_string();
            let processed_urls = Vec::new(); // TODO: attachments via page
            let payload = client.build_conversation_payload(&request, &processed_urls, &conversation_id);

            // Make the API call from within the grok.com page context.
            // The app's own fetch wrapper adds x-statsig-id + Baggage.
            let body_json = serde_json::to_string(&payload)
                .map_err(|e| GatewayError::Internal(format!("grok payload serialize: {e}")))?;
            let js = format!(
                r#"(async function() {{
                    try {{
                        const resp = await fetch('/rest/app-chat/conversations/new', {{
                            method: 'POST',
                            headers: {{ 'Content-Type': 'application/json' }},
                            credentials: 'include',
                            body: {}
                        }});
                        const text = await resp.text();
                        return {{ status: resp.status, body: text.substring(0, 100000) }};
                    }} catch(e) {{
                        return {{ status: 0, error: String(e) }};
                    }}
                }})()"#, serde_json::to_string(&body_json).unwrap_or_default()
            );

            let result_raw = sessions.execute_js(&session.id, &js).await
                .map_err(|e| GatewayError::Provider(format!("grok page fetch failed: {e}")))?;

            let status = result_raw.get("status").and_then(|s| s.as_u64()).unwrap_or(0);
            if status < 200 || status >= 300 {
                let err_text = result_raw.get("error").and_then(|e| e.as_str())
                    .unwrap_or("unknown error");
                return Err(GatewayError::Provider(format!(
                    "grok page transport error (status {status}): {err_text}"
                )));
            }

            let body = result_raw.get("body").and_then(|b| b.as_str()).unwrap_or("");

            // Parse NDJSON stream text.
            let (full_text, reasoning_text, finish_reason) =
                direct::parse_ndjson_body(body);

            let prompt_tokens = crate::providers::tokenizer::estimate_tokens("grok", &request.model, &request.messages.iter().map(|m| m.content.as_text()).collect::<String>());
            let completion_tokens = crate::providers::tokenizer::estimate_tokens("grok", &request.model, &full_text);
            let usage = Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            };

            let session_url = Some(format!("https://grok.com/chat/{conversation_id}"));

            // Store conversation for continuation.
            this.store.get_or_create(request.session_url.as_deref(), &request.model, conversation_id);

            Ok(ChatCompletionResponse {
                id: format!("chatcmpl-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                object: "chat.completion".to_string(),
                created: direct::current_timestamp(),
                model: request.model.clone(),
                choices: vec![crate::models::ChatCompletionChoice {
                    index: 0,
                    message: ChatMessage {
                        role: "assistant".to_string(),
                        content: ChatContent::String(full_text),
                        name: None,
                        reasoning_content: if reasoning_text.is_empty() { None } else { Some(reasoning_text) },
                        citations: None,
                        tool_calls: None,
                        tool_call_id: None,
                    },
                    finish_reason,
                }],
                usage,
                session_url,
            })
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

            // Browser-as-transport proof of concept: navigate to grok.com,
            // then make the API call from within the page context where the
            // app's own interceptors add signed headers automatically.
            sessions.navigate(&session.id, "https://grok.com").await?;
            tracing::info!("grok: navigating to grok.com for browser transport");
            for _ in 0..15 {
                sessions.pump_event_loop(&session.id, 2000).await.ok();
                let state = sessions.execute_js(&session.id,
                    r#"document.readyState + '|' + String(typeof window.__next_f !== 'undefined')"#
                ).await.unwrap_or(serde_json::Value::Null);
                tracing::debug!(state = %state, "grok: page load check");
                if state.as_str().map(|s| s.starts_with("complete|true")).unwrap_or(false) {
                    break;
                }
            }
            tracing::info!("grok: page loaded, testing statsig capture");

            // Trigger a lightweight REST call and capture its headers via
            // XHR interception.
            let hook = r#"(function(){
                window.__capturedStatsig = null;
                const origOpen = XMLHttpRequest.prototype.open;
                const origHeader = XMLHttpRequest.prototype.setRequestHeader;
                XMLHttpRequest.prototype.open = function(m, u) {
                    this._url = u; return origOpen.apply(this, arguments);
                };
                XMLHttpRequest.prototype.setRequestHeader = function(name, value) {
                    if (name.toLowerCase() === 'x-statsig-id') {
                        window.__capturedStatsig = value;
                    }
                    return origHeader.apply(this, arguments);
                };
                // Trigger a small REST call
                const x = new XMLHttpRequest();
                x.open('GET', '/rest/modes');
                x.send();
                return 'hooks installed';
            })()"#;
            let _ = sessions.execute_js(&session.id, hook).await;

            // Wait for the XHR to fire and capture the header.
            for _ in 0..20 {
                sessions.pump_event_loop(&session.id, 500).await.ok();
                let captured = sessions.execute_js(&session.id,
                    "window.__capturedStatsig || null"
                ).await.unwrap_or(serde_json::Value::Null);
                if let Some(tok) = captured.as_str() {
                    if tok.len() > 40 {
                        tracing::info!(statsig_len = tok.len(), "grok: statsig harvested from XHR");
                        // Store it for DirectClient to use.
                        this.store.store_statsig(tok.to_string());
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }

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
