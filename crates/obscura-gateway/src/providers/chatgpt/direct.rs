use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use reqwest::header::CONTENT_TYPE;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{info, warn};
use url::Url;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatMessageDelta, ChunkChoice, Citation, ToolCall, ToolChoice, Usage,
};
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::mtp;
use crate::providers::tool_call::{
    format_tool_results,
};
use crate::providers::send_with_retry;
use crate::session::{SessionHandle, SessionManager};

use super::auth::{build_request_headers, extract_bearer_token, extract_bearer_token_direct, navigate_to_chatgpt, AuthData};
use super::models::resolve_model;
use super::rpc::{
    build_request_payload, generate_proof_token, get_proof_token_config, get_requirements_token,
    parse_codex_sse_line, parse_sse_line,
};
use super::state::{SessionStore, StoredConversation};
use super::upload::{decode_data_uri, upload_files, validate_remote_url};
use crate::providers::streaming_upload::download_and_hash_batch;


const CHATGPT_URL: &str = "https://chatgpt.com";
const TIMEOUT_SECS: u64 = 120;

/// Generate a new session token.
fn new_session_token() -> String {
    format!("chatgpt_{}", uuid::Uuid::new_v4().simple())
}

/// Format: `https://chatgpt.com/c/{conversation_id}#session=<token>`
fn make_session_url(token: &str, conversation_id: &str) -> String {
    format!("{}/c/{}#session={}", CHATGPT_URL, conversation_id, token)
}

/// Extract session token from a session_url.
fn extract_session_token(url: &str) -> Option<String> {
    url.split("#session=").nth(1).map(|s| s.to_string())
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Direct API client for ChatGPT's internal conversation endpoint.
pub struct ChatGptDirectClient {
    http: Client,
    #[allow(dead_code)]
    auth: AuthData,
    store: SessionStore,
    /// User-facing model alias (e.g., "chatgpt-auto").
    model_id: String,
    /// Internal model name used by the API (e.g., "text-davinci-002-render-sha").
    internal_model_id: String,
    session_token: Option<String>,
    /// Cache for thinking-effort PATCH calls, keyed by `"{model_slug}:{effort}"`.
    /// Prevents redundant PATCH requests within the 5-minute TTL.
    thinking_cache: HashMap<String, Instant>,
}

impl ChatGptDirectClient {
    /// Create a new client, authenticating via the warmed browser session.
    ///
    /// Optimized flow to reduce browser navigation overhead:
    /// 1. Try extracting token from cookies/localStorage WITHOUT navigation
    /// 2. If that fails, navigate to chatgpt.com and extract (fallback)
    /// 3. Cache extracted token for reuse within 5-minute TTL
    pub async fn new(
        session: &SessionHandle,
        model_id: &str,
        sessions: &SessionManager,
        store: SessionStore,
    ) -> Result<Self, GatewayError> {
        let model = resolve_model(model_id).ok_or_else(|| {
            GatewayError::BadRequest(format!("unknown ChatGPT model: {model_id}"))
        })?;

        // Optimization: Try to extract token from existing cookies WITHOUT navigation.
        // This avoids the 8-second sleep on every request if session is already at chatgpt.com.
        // If this fails (e.g., session was used by another provider), fall back to navigation.
        let auth = match extract_bearer_token_direct(sessions, &session.id, &session.cookie_jar)
            .await
            .ok()
            .flatten()
        {
            Some(token) => {
                info!(
                    session_id = %session.id,
                    model = %model_id,
                    "ChatGPT token extracted without navigation (cached cookies)"
                );
                AuthData {
                    access_token: token,
                    user_agent: session.user_agent.clone(),
                }
            }
            None => {
                // The startup cookie snapshot may be stale: NextAuth rotates the
                // chatgpt.com session cookie on every refresh, and the gateway
                // only imported it once at boot. Re-snapshot from the live
                // profile (WAL-aware) so the user's current session is picked
                // up, then retry the fast path before paying for navigation.
                info!(
                    session_id = %session.id,
                    model = %model_id,
                    "ChatGPT cookies stale/missing; re-snapshotting live profile and retrying"
                );
                let refreshed = match sessions.refresh_auth().await {
                    Ok(()) => {
                        extract_bearer_token_direct(sessions, &session.id, &session.cookie_jar)
                            .await
                            .ok()
                            .flatten()
                    }
                    Err(e) => {
                        warn!(session_id = %session.id, error = %e, "chatgpt refresh_auth failed; continuing with navigation fallback");
                        None
                    }
                };
                if let Some(token) = refreshed {
                    info!(
                        session_id = %session.id,
                        model = %model_id,
                        "ChatGPT token extracted after live profile re-snapshot"
                    );
                    AuthData {
                        access_token: token,
                        user_agent: session.user_agent.clone(),
                    }
                } else {
                    // Navigation required: page may have been navigated away by another provider
                    // or cookies may be stale. Navigate to chatgpt.com and re-extract token.
                    info!(
                        session_id = %session.id,
                        model = %model_id,
                        "ChatGPT token extraction requires browser navigation (cookies stale/missing)"
                    );
                    navigate_to_chatgpt(sessions, &session.id).await?;
                    extract_bearer_token(sessions, &session.id, &session.cookie_jar).await?
                }
            }
        };

        let chatgpt_url = Url::parse(CHATGPT_URL)
            .map_err(|e| GatewayError::Internal(format!("invalid URL: {e}")))?;
        let cookie_header = session.cookie_jar.get_cookie_header(&chatgpt_url);

        let headers = build_request_headers(
            &auth.access_token,
            &cookie_header,
            &session.user_agent,
        )?;

        let http = Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .default_headers(headers)
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;

        info!(
            session_id = %session.id,
            model = %model_id,
            "ChatGPT DirectClient initialized"
        );

        Ok(Self {
            http,
            auth,
            store,
            model_id: model.id.to_string(),
            internal_model_id: model.internal_id.to_string(),
            session_token: None,
            thinking_cache: HashMap::new(),
        })
    }

    /// Resolve conversation state from a session URL.
    /// Returns the session token and stored conversation, or None for new conversations.
    async fn resolve_conversation(
        &self,
        session_url: &Option<String>,
    ) -> Result<Option<(String, StoredConversation)>, GatewayError> {
        let token = match session_url.as_ref().and_then(|u| extract_session_token(u)) {
            Some(t) => t,
            None => return Ok(None),
        };

        match self.store.acquire(&token).await {
            Some(stored) => {
                SessionStore::ensure_model_matches(&self.model_id, &stored.model_id)?;
                Ok(Some((token, stored)))
            }
            None => Err(GatewayError::BadRequest(format!(
                "invalid or expired session_url: {}",
                session_url.as_deref().unwrap_or("")
            ))),
        }
    }

    /// Store conversation state. Reuses existing token if available.
    /// Store a completed conversation turn as a fresh branch.
    ///
    /// The ChatGPT web backend models a conversation as a message *tree*:
    /// a turn is `conversation_id + parent_message_id`, and two turns that
    /// share the same parent are siblings (branches). To let concurrent
    /// requests continue the same conversation in parallel, every completed
    /// turn mints a brand-new branch token keyed to
    /// `(conversation_id, message_id)` instead of reusing the incoming one.
    /// A client that continues the returned `session_url` follows exactly
    /// that branch; two requests started from the same `session_url` fork at
    /// the same parent and each get an independent, continuable tip.
    ///
    /// `existing_token` is ignored for branch identity; it is kept in the
    /// signature so callers that previously threaded a continuation token
    /// through need no structural change.
    async fn store_conversation(
        &self,
        conv: &StoredConversation,
        existing_token: Option<&str>,
    ) -> String {
        let _ = existing_token;
        let token = new_session_token();
        self.store.insert(token.clone(), conv, &self.model_id).await;
        make_session_url(&token, &conv.conversation_id)
    }

    /// Chat requirements + PoW + conversation request pipeline.
    ///
    /// This performs:
    /// 1. POST `/backend-api/sentinel/chat-requirements` — get PoW challenge + chat token
    /// 2. Solve PoW (SHA3-512) if required
    /// 3. POST `/backend-api/f/conversation` — send messages with PoW proof
    async fn send_conversation_request(
        &self,
        _prompt: &str,
        conv: Option<&StoredConversation>,
        images: Option<&serde_json::Value>,
        messages: &[ChatMessage],
        parent_message_id: &str,
        _for_stream: bool,
        search: Option<bool>,
        request: &ChatCompletionRequest,
    ) -> Result<reqwest::Response, GatewayError> {
        let start = std::time::Instant::now();

        // Step 0: Obtain conduit token (3-phase sentinel)
        // POST to /backend-api/f/conversation/prepare with x-conduit-token: no-token
        // The response header x-conduit-token is the real token to use on the conversation request.
        // Continuation requests must also carry the conversation_id in the
        // body; the backend keys the conduit handshake off it (gpt4free PR
        // #3343) and omitting it breaks multi-turn continuity.
        let prepare_url = format!("{}/backend-api/f/conversation/prepare", CHATGPT_URL);
        let prepare_req = self.http.post(&prepare_url);
        let prepare_req = if let Some(conv) = conv {
            if !conv.conversation_id.is_empty() {
                prepare_req.json(&serde_json::json!({
                    "conversation_id": conv.conversation_id
                }))
            } else {
                prepare_req
            }
        } else {
            prepare_req
        };
        let prepare_resp = prepare_req
            .header("x-conduit-token", "no-token")
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("conversation prepare failed: {e}")))?;
        tracing::info!(ms = %start.elapsed().as_millis(), "chatgpt step0 prepare done");

        let conduit_token = prepare_resp
            .headers()
            .get("x-conduit-token")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();

        // Step 1: Get chat requirements (PoW challenge)
        let proof_config = get_proof_token_config("Mozilla/5.0");
        let req_token = get_requirements_token(&proof_config);
        let req_url = format!("{}/backend-api/sentinel/chat-requirements", CHATGPT_URL);

        let req_resp = self
            .http
            .post(&req_url)
            .json(&serde_json::json!({"p": req_token}))
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("chat-requirements request failed: {e}")))?;

        let req_status = req_resp.status();
        let req_body: serde_json::Value = req_resp
            .json()
            .await
            .map_err(|e| GatewayError::Provider(format!("chat-requirements decode failed: {e}")))?;
        tracing::info!(ms = %start.elapsed().as_millis(), status = %req_status, "chatgpt step1 chat-requirements done");

        if !req_status.is_success() {
            return Err(GatewayError::Provider(format!(
                "chat-requirements returned {req_status}: {req_body}"
            )));
        }

        let chat_token = req_body["token"]
            .as_str()
            .ok_or_else(|| GatewayError::Provider("no token in chat-requirements response".to_string()))?
            .to_string();

        // Step 2: Solve PoW if required
        let proof_token = if let Some(pow) = req_body.get("proofofwork") {
            let required = pow.get("required").and_then(|v| v.as_bool()).unwrap_or(false);
            let seed = pow.get("seed").and_then(|v| v.as_str()).unwrap_or("");
            let difficulty = pow.get("difficulty").and_then(|v| v.as_str()).unwrap_or("");
            let pow_result = generate_proof_token(required, seed, difficulty);
            tracing::info!(ms = %start.elapsed().as_millis(), required, "chatgpt step2 pow solved");
            pow_result
        } else {
            tracing::info!(ms = %start.elapsed().as_millis(), "chatgpt step2 no pow required");
            None
        };

        // Step 3: Build the conversation request
        let mut payload = build_request_payload(messages, conv, images, &self.internal_model_id, parent_message_id, search, &request);

        // TEST HARNESS: CHATGPT_TEST env var controls payload variations
        let test_mode = std::env::var("CHATGPT_TEST").ok();
        if let Some(ref mode) = test_mode {
            match mode.as_str() {
                "none" => {
                    payload.as_object_mut().and_then(|o| o.remove("tools"));
                }
                "tools_auto" => {
                    payload["tool_choice"] = serde_json::json!("auto");
                }
                "tools_required" => {
                    payload["tool_choice"] = serde_json::json!("required");
                }
                "conv_tool" => {
                    payload["conversation_mode"] = serde_json::json!({"kind": "function_call"});
                }
                "prompt_only" => {
                    payload.as_object_mut().and_then(|o| o.remove("tools"));
                }
                _ => {}
            }
            let tc_str = payload.get("tool_choice").map(|v| v.to_string()).unwrap_or_default();
            let cm_str = payload.get("conversation_mode").map(|v| v.to_string()).unwrap_or_default();
            tracing::info!(test_mode = %mode, payload_tools = %payload.get("tools").map(|t| t.to_string()).unwrap_or_default().chars().take(200).collect::<String>(), payload_tool_choice = %tc_str, payload_mode = %cm_str, "test mode applied");
        }


        let url = format!("{}/backend-api/f/conversation", CHATGPT_URL);

        let mut req_builder = self.http.post(&url).json(&payload);

        // Add PoW headers
        if let Some(ref proof) = proof_token {
            req_builder = req_builder.header("openai-sentinel-proof-token", proof.as_str());
        }
        req_builder = req_builder.header("openai-sentinel-chat-requirements-token", &chat_token);

        // Add conduit token if available
        if !conduit_token.is_empty() {
            req_builder = req_builder.header("openai-conduit-token", &conduit_token);
        }

        req_builder = req_builder.header("accept", "text/event-stream");

        let resp = req_builder
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("conversation request failed: {e}")))?;
        tracing::info!(ms = %start.elapsed().as_millis(), status = %resp.status(), "chatgpt step3 conversation post done");

        Ok(resp)
    }

    /// Set the model slug (and optionally thinking effort) via the PATCH
    /// settings endpoint.
    ///
    /// Sends `PATCH /backend-api/settings/user_last_used_model_config`
    /// with `model_slug=` on every request so the backend uses the
    /// correct model. When `enabled` is true, also appends
    /// `&thinking_effort=extended`.
    ///
    /// Results are cached per `(model_slug, effort)` for 5 minutes
    /// so a rapid sequence of calls does not hammer the endpoint.
    ///
    /// Non-fatal: if the PATCH fails, a warning is logged and the
    /// conversation proceeds.
    async fn set_model_and_thinking(&mut self, model_slug: &str, enabled: bool) {
        let effort_str = if enabled { "extended" } else { "default" };
        let cache_key = format!("{}:{}", model_slug, effort_str);
        if self.thinking_cache.get(&cache_key).is_some_and(|expires| *expires > Instant::now()) {
            return;
        }

        let url = if enabled {
            format!(
                "{}/backend-api/settings/user_last_used_model_config?model_slug={}&thinking_effort=extended",
                CHATGPT_URL,
                model_slug,
            )
        } else {
            format!(
                "{}/backend-api/settings/user_last_used_model_config?model_slug={}",
                CHATGPT_URL,
                model_slug,
            )
        };

        let resp = self
            .http
            .patch(&url)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                self.thinking_cache.insert(cache_key, Instant::now() + Duration::from_secs(300));
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default().chars().take(500).collect::<String>();
                tracing::warn!(
                    %status,
                    body = %body,
                    model = %model_slug,
                    "model-config PATCH returned non-success"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    model = %model_slug,
                    "model-config PATCH failed"
                );
            }
        }
    }

    /// Send a request to the `/backend-api/codex/responses` endpoint.
    ///
    /// Unlike the conversation endpoint, this supports native tool calling
    /// with proper `/message/tool_calls`-style events. Uses the same bearer
    /// token auth as other ChatGPT endpoints.
    #[allow(dead_code)]
    async fn send_codex_request(
        &self,
        payload: &serde_json::Value,
    ) -> Result<reqwest::Response, GatewayError> {
        let url = format!("{}/backend-api/codex/responses", CHATGPT_URL);
        let builder = self
            .http
            .post(&url)
            .header("Originator", "codex_cli_rs")
            .header("OpenAI-Beta", "responses=experimental")
            .json(payload);

        send_with_retry(builder)
            .await
            .map_err(|e| GatewayError::Provider(format!("codex/responses request failed: {e}")))
    }

    /// Parse SSE content from a non-streaming response into final text and thinking.
    /// Returns `(content_text, thinking_text)` where thinking is the accumulated
    /// reasoning content (o-series models only).
    fn collect_text_from_sse(body: &[u8]) -> (String, Option<String>, Option<Vec<ToolCall>>) {
        let text = String::from_utf8_lossy(body);
        let mut content = String::new();
        let mut thinking = String::new();
        let mut collected_tool_calls: Vec<ToolCall> = Vec::new();
        for line in text.lines() {
            if let Some((delta, _, _, _, think_delta, tool_calls_json)) = parse_sse_line(line, "") {
                if let Some(t) = delta {
                    content.push_str(&t);
                }
                if let Some(t) = think_delta {
                    thinking.push_str(&t);
                }
                if let Some(calls) = tool_calls_json {
                    for call in calls {
                        let id = match call.get("id").and_then(|v| v.as_str()) {
                            Some(id) if !id.is_empty() => id.to_string(),
                            _ => continue,
                        };
                        let name = call.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let args = call.get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        collected_tool_calls.push(ToolCall {
                            id,
                            r#type: "function".to_string(),
                            function: crate::models::FunctionCall {
                                name: name.to_string(),
                                arguments: args.to_string(),
                            },
                        });
                    }
                }
            }
        }
        let thinking = if thinking.is_empty() { None } else { Some(thinking) };
        let tool_calls = if collected_tool_calls.is_empty() { None } else { Some(collected_tool_calls) };
        (content, thinking, tool_calls)
    }

    /// Parse a non-streaming response from the codex/responses endpoint.
    ///
    /// Handles both JSON format (non-streaming) and SSE format (streaming).
    /// Returns `(body, conversation_id, message_id, text, thinking, tool_calls)`.
    #[allow(dead_code)]
    fn parse_codex_response(
        body: &[u8],
    ) -> (
        Vec<u8>,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<Vec<ToolCall>>,
    ) {
        let body_str = String::from_utf8_lossy(body);

        // Try JSON format first (non-streaming response)
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body_str) {
            let resp_id = json.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let output = json.get("output").and_then(|v| v.as_array());

            let mut text = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();

            if let Some(items) = output {
                for item in items {
                    match item.get("type").and_then(|v| v.as_str()) {
                        Some("message") => {
                            if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                                for part in content {
                                    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                        text.push_str(t);
                                    }
                                }
                            }
                        }
                        Some("function_call") => {
                            let id = item.get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = item.get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = item.get("arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or("{}")
                                .to_string();
                            tool_calls.push(ToolCall {
                                id,
                                r#type: "function".to_string(),
                                function: crate::models::FunctionCall {
                                    name,
                                    arguments: args,
                                },
                            });
                        }
                        _ => {}
                    }
                }
            }

            let tool_calls = if tool_calls.is_empty() { None } else { Some(tool_calls) };
            let reasoning = json.get("reasoning")
                .or_else(|| json.get("thinking"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return (body.to_vec(), resp_id.clone(), resp_id, text, reasoning, tool_calls);
        }

        // Fallback: try SSE format
        let mut text = String::new();
        let mut thinking: Option<String> = None;
        let mut tool_calls_args: std::collections::HashMap<String, (String, String, String)> = std::collections::HashMap::new();
        for line in body_str.lines() {
            // Try codex SSE format first
            if let Some((delta, think_delta, tool_call_value)) = parse_codex_sse_line(line) {
                if let Some(t) = delta {
                    text.push_str(&t);
                }
                if let Some(t) = think_delta {
                    thinking.get_or_insert_with(String::new).push_str(&t);
                }
                if let Some(v) = tool_call_value {
                    // function_call_arguments delta
                    if let Some(args_delta) = v.as_str() {
                        // Accumulate arguments - use index 0 as default
                        if let Some(entry) = tool_calls_args.get_mut("0") {
                            entry.2.push_str(args_delta);
                        }
                    }
                    // output_item.added or output_item.done
                    if let Some(obj) = v.as_object() {
                        if let Some(call_id) = obj.get("call_id").and_then(|v| v.as_str()) {
                            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let args = obj.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                            let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            tool_calls_args.insert(
                                id.to_string(),
                                (call_id.to_string(), name.to_string(), args.to_string()),
                            );
                        }
                    }
                }
                continue;
            }
            // Fallback: try regular conversation SSE
            if let Some((delta, _, _, _, think_delta, tool_calls_json)) = parse_sse_line(line, &text) {
                if let Some(t) = delta {
                    text.push_str(&t);
                }
                if let Some(t) = think_delta {
                    thinking.get_or_insert_with(String::new).push_str(&t);
                }
                if let Some(calls) = tool_calls_json {
                    for call in calls {
                        let id = call.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = call.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let args = call.get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        tool_calls_args.insert(
                            id.clone(),
                            (id.clone(), name.to_string(), args.to_string()),
                        );
                    }
                }
            }
        }

        let tool_calls: Vec<ToolCall> = tool_calls_args
            .into_values()
            .map(|(id, name, args)| ToolCall {
                id,
                r#type: "function".to_string(),
                function: crate::models::FunctionCall { name, arguments: args },
            })
            .collect();
        let tool_calls = if tool_calls.is_empty() { None } else { Some(tool_calls) };

        (body.to_vec(), None, None, text, thinking, tool_calls)
    }

    /// Handle tool-result messages: look up stored tool calls and format results.
    async fn handle_tool_results(
        &self,
        request: &ChatCompletionRequest,
        session_token: Option<&str>,
    ) -> Vec<ChatMessage> {
        let tool_msgs: Vec<&ChatMessage> = request
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .collect();

        if tool_msgs.is_empty() {
            return Vec::new();
        }

        let token = match session_token {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut items: Vec<(Option<ToolCall>, Option<String>, String)> = Vec::new();
        for msg in &tool_msgs {
            let call = match msg.tool_call_id.as_deref() {
                Some(id) => self.store.get_tool_call(token, id).await,
                None => None,
            };
            items.push((call, msg.tool_call_id.clone(), msg.content.as_text()));
        }

        let refs: Vec<(Option<&ToolCall>, Option<&str>, &str)> = items
            .iter()
            .map(|(c, id, o)| (c.as_ref(), id.as_deref(), o.as_str()))
            .collect();

        let formatted = format_tool_results(&refs);
        if formatted.is_empty() {
            Vec::new()
        } else {
            vec![ChatMessage {
                role: "tool".to_string(),
                content: crate::models::ChatContent::String(formatted),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            }]
        }
    }

    /// Inject tool definitions as a system message so the model knows what tools
    /// it can call. When `tool_choice="required"`, the instruction is forceful.
    fn inject_tool_definitions(messages: &mut Vec<ChatMessage>, request: &ChatCompletionRequest) {
        let Some(tools) = request.tools.as_ref() else {
            return;
        };
        if tools.is_empty() {
            return;
        }

        // Universal MTP/1 dialect: compile tools into the prompted
        // tool-output protocol (replaces the legacy <tool_call> hint).
        let instruction =
            mtp::build_mtp_system_prompt(tools, request.tool_choice.as_ref(), false, mtp::prompt_style_for_model(&request.model));

        messages.insert(0, ChatMessage {
            role: "system".to_string(),
            content: crate::models::ChatContent::String(instruction),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }

    /// Resolve a single image URL to (bytes, filename, mime_type).
    /// Handles both data: URIs and regular HTTP(S) URLs.
    #[allow(dead_code)]
    async fn resolve_image_url(&self, url: &str) -> Result<(Vec<u8>, String, String), GatewayError> {
        if let Some((bytes, mime)) = decode_data_uri(url) {
            let ext = match mime.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                "image/gif" => "gif",
                _ => "png",
            };
            return Ok((bytes, format!("image.{}", ext), mime));
        }

        validate_remote_url(url)?;

        let resp = self.http.get(url).send().await
            .map_err(|e| GatewayError::Internal(format!("image fetch failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(GatewayError::Internal(format!("image fetch returned {}", resp.status())));
        }

        let mime = resp.headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).to_string())
            .unwrap_or_else(|| "image/png".to_string());

        let bytes = resp.bytes().await
            .map_err(|e| GatewayError::Internal(format!("image read failed: {e}")))?
            .to_vec();

        let name = url::Url::parse(url)
            .ok()
            .and_then(|u| u.path_segments().and_then(|s| s.last().map(|s| s.to_string())))
            .unwrap_or_else(|| "image.png".to_string());

        Ok((bytes, name, mime))
    }

    /// Prepare images from messages: download, upload to ChatGPT, return JSON array
    /// of image metadata objects. Each object contains:
    ///   `asset_pointer`: the `image_asset_pointer` payload for the conversation parts
    ///   `attachment`: the attachment metadata for `message.metadata.attachments`
    ///
    /// Uses concurrent streaming downloads for parallel processing without buffering
    /// entire files into memory.
    async fn prepare_images(&self, messages: &[ChatMessage]) -> Result<Option<serde_json::Value>, GatewayError> {
        let mut urls: Vec<String> = messages.iter()
            .flat_map(|m| m.content.image_urls())
            .collect();

        if urls.is_empty() {
            return Ok(None);
        }

        // Separate data URIs from remote URLs (data URIs are already decoded)
        let mut files: Vec<(Vec<u8>, String, String)> = Vec::new();
        let mut remote_urls: Vec<String> = Vec::new();

        for url in urls.drain(..) {
            if let Some((bytes, mime)) = decode_data_uri(&url) {
                let ext = match mime.as_str() {
                    "image/png" => "png",
                    "image/jpeg" => "jpg",
                    "image/webp" => "webp",
                    "image/gif" => "gif",
                    _ => "png",
                };
                files.push((bytes, format!("image.{}", ext), mime));
            } else {
                remote_urls.push(url);
            }
        }

        // Concurrently download and hash remote URLs (up to 4 in parallel)
        if !remote_urls.is_empty() {
            let hashed = download_and_hash_batch(&self.http, remote_urls).await?;
            for hashed_file in hashed {
                files.push((
                    hashed_file.bytes,
                    hashed_file.name,
                    hashed_file.mime_type,
                ));
            }
        }

        // Upload all files to ChatGPT
        let uploaded = upload_files(&self.http, &files, &self.store.upload_cache).await?;

        let arr: Vec<serde_json::Value> = uploaded.iter()
            .map(|f| serde_json::json!({
                "asset_pointer": f.to_asset_pointer(),
                "attachment": f.to_attachment(),
            }))
            .collect();

        Ok(Some(serde_json::Value::Array(arr)))
    }

    /// Non-streaming chat completion.
    pub async fn chat(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        // Resolve conversation from session_url (no lock held).
        let (conv_token, stored_conv) =
            match self.resolve_conversation(&request.session_url).await? {
                Some((token, stored)) => {
                    self.session_token = Some(token.clone());
                    (Some(token), Some(stored))
                }
                None => (None, None),
            };

        let last_msg = request.messages.last();
        let prompt = last_msg.map(|m| m.content.as_text()).unwrap_or_default();

        // Handle tool results
        let tool_messages = self
            .handle_tool_results(&request, conv_token.as_deref())
            .await;

        // Build the full message list (system messages first, then history, then user)
        let mut messages = request.messages.clone();
        if !tool_messages.is_empty() {
            messages.extend(tool_messages);
        }

        // Inject tool definitions into the last user message so the model
        // knows what tools it may call. The chatgpt.com web endpoint
        // (/backend-api/f/conversation) does not produce /message/tool_calls
        // SSE events for native tool fields, so we always use prompt
        // injection regardless of whether native tools are also sent.
        // inject_tool_definitions returns early if tools is None or empty.
        // Skip when CHATGPT_TEST is set to a payload-only mode.
        let skip_inject = std::env::var("CHATGPT_TEST").ok().map_or(false, |m| matches!(m.as_str(), "none" | "tools_only" | "tools_auto" | "tools_required" | "conv_tool"));
        if !skip_inject {
            Self::inject_tool_definitions(&mut messages, &request);
        }

        // Prepare images: download and upload to ChatGPT, get download URLs
        let images = self.prepare_images(&messages).await?;

        let parent_message_id = stored_conv
            .as_ref()
            .map(|c| c.message_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Set thinking effort via PATCH before the conversation request
        let model_slug = self.internal_model_id.clone();
        self.set_model_and_thinking(&model_slug, request.thinking == Some(true)).await;

        let (_body, conversation_id, message_id, clean_text, thinking, parsed_tool_calls) =
            {
                let resp = self
                    .send_conversation_request(
                        &prompt,
                        stored_conv.as_ref(),
                        images.as_ref(),
                        &messages,
                        &parent_message_id,
                        false,
                        request.search,
                        &request,
                    )
                    .await?;

                let status = resp.status();
                let body = resp
                    .bytes()
                    .await
                    .map_err(|e| GatewayError::Provider(format!("response read failed: {e}")))?;

                tracing::info!(%status, body_len = body.len(), "conversation response received");

                // Log response metadata for test harness
                let body_str_check = String::from_utf8_lossy(&body);
                if let Some(meta_start) = body_str_check.find("server_ste_metadata") {
                    let before = &body_str_check[..meta_start];
                    let brace_open = before.rfind('{').unwrap_or(0);
                    let after = &body_str_check[meta_start..];
                    let mut depth = 1i32; // opening { is before meta_start
                    let mut brace_close = 0;
                    for (i, c) in after.chars().enumerate() {
                        if c == '{' { depth += 1; }
                        else if c == '}' { depth -= 1; }
                        if depth == 0 { brace_close = meta_start + i; break; }
                    }
                    if brace_close > meta_start {
                        let meta_str = &body_str_check[brace_open..=brace_close];
                        if let Ok(meta_val) = serde_json::from_str::<serde_json::Value>(meta_str) {
                            let m = &meta_val["metadata"];
                            tracing::info!(
                                plan_type = %m.get("plan_type_bucket").or_else(|| m.get("plan_type")).and_then(|v| v.as_str()).unwrap_or("?"),
                                turn_use_case = %m.get("turn_use_case").and_then(|v| v.as_str()).unwrap_or("?"),
                                tool_invoked = %m.get("tool_invoked").map(|v| v.to_string()).unwrap_or_else(|| "?".to_string()),
                                tool_name = %m.get("tool_name").and_then(|v| v.as_str()).unwrap_or("null"),
                                model_slug = %m.get("model_slug").and_then(|v| v.as_str()).unwrap_or("?"),
                                resolved_model = %m.get("resolved_model_slug").or_else(|| m.get("model_slug")).and_then(|v| v.as_str()).unwrap_or("?"),
                                turn_mode = %m.get("turn_mode").and_then(|v| v.as_str()).unwrap_or("?"),
                                "response metadata"
                            );
                        } else {
                            tracing::warn!("failed to parse metadata JSON from body");
                        }
                    } else {
                        tracing::warn!("metadata brace matching failed: close={brace_close} start={meta_start}");
                    }
                } else {
                    tracing::warn!("server_ste_metadata NOT FOUND in response body");
                }

                if !status.is_success() {
                    return Err(GatewayError::Provider(format!(
                        "ChatGPT API returned {status}: {}",
                        String::from_utf8_lossy(&body).chars().take(200).collect::<String>()
                    )));
                }

                // Parse SSE content, thinking, and native tool calls
                let (text, thinking, native_tool_calls) = Self::collect_text_from_sse(&body);

                // Empty 200 with a non-empty body is the drift signature;
                // snapshot for healing before the body is dropped.
                if text.is_empty() && thinking.as_deref().is_none() && !body.is_empty() {
                    crate::providers::drift_snapshot::global()
                        .record("chatgpt", "empty-200", &body);
                }

                tracing::debug!(text_len = text.len(), text_snippet = %text.chars().take(200).collect::<String>(), has_native = native_tool_calls.is_some(), "SSE parsed text");

                // Prefer native tool calls from SSE, fall back to MTP blocks
                let (clean_text, parsed_tool_calls) = if let Some(calls) = native_tool_calls {
                    tracing::debug!(call_count = calls.len(), "using native tool calls from SSE");
                    (text, Some(calls))
                } else {
                    let defs: Vec<crate::models::Tool> = request.tools.clone().unwrap_or_default();
                    let mut st = mtp::MtpStreamState::new();
                    let stripped = st.process_delta(&text, &defs);
                    st.finish(&defs);
                    let calls = std::mem::take(&mut st.collected_tool_calls);
                    if calls.is_empty() { (text, None) } else { (stripped, Some(calls)) }
                };

                // Extract conversation_id from the response
                let body_str = String::from_utf8_lossy(&body);
                let mut conversation_id: Option<String> = None;
                let mut message_id: Option<String> = None;
                for line in body_str.lines() {
                    if let Some((_, _, conv_id, msg_id, _, _)) = parse_sse_line(line, "") {
                        if conv_id.is_some() {
                            conversation_id = conv_id;
                        }
                        if msg_id.is_some() {
                            message_id = msg_id;
                        }
                    }
                }

                (body.to_vec(), conversation_id, message_id, clean_text, thinking, parsed_tool_calls)
            };

        let has_tool_calls = parsed_tool_calls.is_some();

        // Enforce tool_choice: "required" — the ChatGPT web endpoint ignores
        // native tool_choice, so if injection didn't work, return an error.
        if !has_tool_calls {
            if let Some(ToolChoice::Mode(m)) = &request.tool_choice {
                if m == "required" {
                    return Err(GatewayError::Provider(
                        "tool_choice was set to 'required' but the model produced a text response instead of a tool call. The ChatGPT web endpoint does not support native tool_choice enforcement; prompt injection was used as a fallback."
                            .to_string(),
                    ));
                }
            }
        }

        // Store conversation state for multi-turn support
        let session_url = if let (Some(conv_id), Some(msg_id)) = (&conversation_id, &message_id) {
            let conv = StoredConversation {
                conversation_id: conv_id.clone(),
                message_id: msg_id.clone(),
                model_id: self.model_id.clone(),
            };
            let url = self.store_conversation(&conv, conv_token.as_deref()).await;

            // Persist tool calls so the next `role: "tool"` continuation
            // can look them up.
            if let Some(ref calls) = parsed_tool_calls {
                if !calls.is_empty() {
                    if let Some(token) = extract_session_token(&url) {
                        self.store.store_tool_calls(&token, calls).await;
                    }
                }
            }

            Some(url)
        } else {
            None
        };

        let prompt_text: String = request.messages.iter().map(|m| m.content.as_text()).collect();

        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: self.model_id.clone(),
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: crate::models::ChatContent::String(clean_text.clone()),
                    name: None,
                    reasoning_content: thinking,
                    citations: None,
                    tool_calls: parsed_tool_calls,
                    tool_call_id: None,
                },
                finish_reason: if has_tool_calls {
                    "tool_calls".to_string()
                } else {
                    "stop".to_string()
                },
            }],
            usage: Usage {
                prompt_tokens: estimate_tokens("chatgpt", &self.model_id, &prompt_text),
                completion_tokens: estimate_tokens("chatgpt", &self.model_id, &clean_text),
                total_tokens: estimate_tokens("chatgpt", &self.model_id, &prompt_text)
                    + estimate_tokens("chatgpt", &self.model_id, &clean_text),
            },
            session_url,
        })
    }

    /// Streaming chat completion.
    pub async fn chat_stream(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        // Resolve conversation from session_url (no lock held - allow concurrent requests).
        let (conv_token, stored_conv) =
            match self.resolve_conversation(&request.session_url).await? {
                Some((token, stored)) => {
                    self.session_token = Some(token.clone());
                    (Some(token), Some(stored))
                }
                None => (None, None),
            };

        let last_msg = request.messages.last();
        let _prompt = last_msg.map(|m| m.content.as_text()).unwrap_or_default();

        // Handle tool results
        let tool_messages = self
            .handle_tool_results(&request, conv_token.as_deref())
            .await;

        let mut messages = request.messages.clone();
        if !tool_messages.is_empty() {
            messages.extend(tool_messages);
        }

        // Inject tool definitions into the last user message so the model
        // knows what tools it may call. Tool calls are recovered from
        // `<tool_call>...</tool_call>` markers in the streamed text after
        // the response completes (see post-loop handling below).
        // The chatgpt.com web endpoint does not produce /message/tool_calls
        // SSE events for native tool fields, so prompt injection is always used.
        // Skip when CHATGPT_TEST is set to a payload-only mode.
        let skip_inject = std::env::var("CHATGPT_TEST").ok().map_or(false, |m| matches!(m.as_str(), "none" | "tools_only" | "tools_auto" | "tools_required" | "conv_tool"));
        if !skip_inject {
            Self::inject_tool_definitions(&mut messages, &request);
        }

        // Prepare images: download and upload to ChatGPT, get download URLs
        let images = self.prepare_images(&messages).await?;

        let parent_message_id = stored_conv
            .as_ref()
            .map(|c| c.message_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Set thinking effort via PATCH before spawning the stream task
        let model_slug = self.internal_model_id.clone();
        self.set_model_and_thinking(&model_slug, request.thinking == Some(true)).await;

        // We need to track whether the request had tools so the post-loop
        // pass can decide whether to run `<tool_call>` extraction. A flag
        // is moved into the spawned task alongside `messages`.
        let had_tools = request.tools.is_some();
        let search = request.search;

        let model_id = self.internal_model_id.clone();
        let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());

        let http_client = self.http.clone();
        let store = self.store.clone();
        // Continuation conversation_id for the conduit prepare handshake
        // (gpt4free PR #3343); empty when this is a brand-new conversation.
        let continue_conversation_id = stored_conv
            .as_ref()
            .map(|c| c.conversation_id.clone())
            .unwrap_or_default();
        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();
        let counter = AtomicU32::new(0);

        // Move prepared images into the spawned task
        let images_for_stream = images.clone();

        tokio::spawn(async move {
            // No per-session lock held - allow concurrent requests to same conversation.
            // Session consistency is managed by provider-level optimistic concurrency.

            // Step 0: Obtain conduit token (3-phase sentinel)
            let prepare_url = format!("{}/backend-api/f/conversation/prepare", CHATGPT_URL);
            let prepare_req = http_client.post(&prepare_url);
            let prepare_req = if !continue_conversation_id.is_empty() {
                prepare_req.json(&serde_json::json!({
                    "conversation_id": continue_conversation_id
                }))
            } else {
                prepare_req
            };
            let conduit_token = match prepare_req
                .header("x-conduit-token", "no-token")
                .send()
                .await
            {
                Ok(r) => r
                    .headers()
                    .get("x-conduit-token")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };

            // Step 1: Get chat requirements
            let req_url = format!("{}/backend-api/sentinel/chat-requirements", CHATGPT_URL);
            let proof_config = get_proof_token_config("Mozilla/5.0");
            let req_token = get_requirements_token(&proof_config);

            let req_resp = match http_client
                .post(&req_url)
                .json(&serde_json::json!({"p": req_token}))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(GatewayError::Provider(format!(
                        "chat-requirements request failed: {e}"
                    ))));
                    return;
                }
            };

            let req_body = match req_resp.json::<serde_json::Value>().await {
                Ok(body) => body,
                Err(e) => {
                    let _ = tx.send(Err(GatewayError::Provider(format!(
                        "chat-requirements decode failed: {e}"
                    ))));
                    return;
                }
            };

            let chat_token = req_body["token"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_default();

            if chat_token.is_empty() {
                let _ = tx.send(Err(GatewayError::Provider(
                    "no token in chat-requirements response".to_string(),
                )));
                return;
            }

            // Step 2: Solve PoW if required
            let proof_token = if let Some(pow) = req_body.get("proofofwork") {
                let required = pow.get("required").and_then(|v| v.as_bool()).unwrap_or(false);
                let seed = pow.get("seed").and_then(|v| v.as_str()).unwrap_or("");
                let difficulty = pow.get("difficulty").and_then(|v| v.as_str()).unwrap_or("");
                generate_proof_token(required, seed, difficulty)
            } else {
                None
            };

            // Build the conversation request payload inline
            let payload = build_request_payload(
                &messages,
                stored_conv.as_ref(),
                images_for_stream.as_ref(),
                &model_id,
                &parent_message_id,
                search,
                &request,
            );

            let url = format!("{}/backend-api/f/conversation", CHATGPT_URL);
            let mut req_builder = http_client
                .post(&url)
                .json(&payload)
                .header("accept", "text/event-stream")
                .header("openai-sentinel-chat-requirements-token", &chat_token);

            if !conduit_token.is_empty() {
                req_builder = req_builder.header("openai-conduit-token", &conduit_token);
            }

            if let Some(ref proof) = proof_token {
                req_builder = req_builder.header("openai-sentinel-proof-token", proof.as_str());
            }

            let resp = match send_with_retry(req_builder).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(GatewayError::Provider(format!(
                        "conversation request failed: {e}"
                    ))));
                    return;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let _ = tx.send(Err(GatewayError::Provider(format!(
                    "ChatGPT API returned {status}: {}",
                    body.chars().take(200).collect::<String>()
                ))));
                return;
            }

            // Step 3: Parse SSE stream
            let mut stream = resp.bytes_stream();
            let mut buffer = Vec::new();
            let mut previous_text = String::new();
            let mut emitted_role = false;
            let mut collected_tool_calls: Vec<ToolCall> = Vec::new();
            let mut session_url: Option<String> = None;
            let mut stored_conv_id: Option<String> = None;
            let mut stored_msg_id: Option<String> = None;
            let mut conv_token = conv_token.clone();
            let tool_defs_stream: Vec<crate::models::Tool> =
                request.tools.clone().unwrap_or_default();
            let mut mtp_state = mtp::MtpStreamState::new();

            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(GatewayError::Provider(format!(
                            "ChatGPT streaming read error: {e}"
                        ))));
                        return;
                    }
                };

                buffer.extend_from_slice(&bytes);

                let mut consumed = 0;
                for (i, &byte) in buffer.iter().enumerate() {
                    if byte == b'\n' {
                        let line_bytes = &buffer[consumed..i];
                        consumed = i + 1;

                        if line_bytes.is_empty() {
                            continue;
                        }

                        let line = match std::str::from_utf8(line_bytes) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };

                        match parse_sse_line(line.trim(), &previous_text) {
                            Some((delta, citations, conv_id, msg_id, think_delta, tool_calls)) => {
                                // Track conversation/message IDs
                                if let Some(id) = conv_id {
                                    stored_conv_id = Some(id);
                                }
                                if let Some(id) = msg_id {
                                    stored_msg_id = Some(id);
                                }

                                // Build session URL on first conversation_id.
                                // Mint a fresh branch token rather than
                                // reusing the incoming one so concurrent
                                // continuations from the same parent fork into
                                // independent, continuable branches (ChatGPT
                                // web message-tree semantics).
                                if session_url.is_none() {
                                    if let Some(ref conv_id) = stored_conv_id {
                                        let token = new_session_token();
                                        let stored = StoredConversation {
                                            conversation_id: conv_id.clone(),
                                            message_id: stored_msg_id
                                                .clone()
                                                .unwrap_or_default(),
                                            model_id: model_id.clone(),
                                        };
                                        store.insert(token.clone(), &stored, &model_id).await;
                                        conv_token = Some(token.clone());
                                        session_url = Some(make_session_url(&token, conv_id));
                                    }
                                }

                                // Convert native tool call values to ToolCall structs
                                let parsed_tool_calls: Option<Vec<ToolCall>> = tool_calls.map(|calls| {
                                    calls.iter().filter_map(|call| {
                                        let id = call.get("id").and_then(|v| v.as_str())?;
                                        let name = call.get("function")
                                            .and_then(|f| f.get("name"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        let args = call.get("function")
                                            .and_then(|f| f.get("arguments"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("{}");
                                        Some(ToolCall {
                                            id: id.to_string(),
                                            r#type: "function".to_string(),
                                            function: crate::models::FunctionCall {
                                                name: name.to_string(),
                                                arguments: args.to_string(),
                                            },
                                        })
                                    }).collect()
                                }).filter(|c: &Vec<ToolCall>| !c.is_empty());

                                // Strip prompt-injected MTP tool blocks from
                                // the delta during streaming so raw markers
                                // never reach the client. Complete validated
                                // calls are emitted as structured chunks.
                                let (clean_delta, mtp_calls) = if had_tools {
                                    if let Some(d) = &delta {
                                        let clean = mtp_state.process_delta(d, &tool_defs_stream);
                                        let calls =
                                            std::mem::take(&mut mtp_state.collected_tool_calls);
                                        (clean, calls)
                                    } else {
                                        (String::new(), Vec::new())
                                    }
                                } else {
                                    (delta.unwrap_or_default(), Vec::new())
                                };

                                for tc in mtp_calls {
                                    if !collected_tool_calls.iter().any(|c| c.id == tc.id) {
                                        collected_tool_calls.push(tc.clone());
                                        let idx = counter.fetch_add(1, Ordering::Relaxed);
                                        let _ = tx.send(Ok(ChatCompletionChunk {
                                            id: format!("{}-{}", id_prefix, idx),
                                            object: "chat.completion.chunk".to_string(),
                                            created: current_timestamp(),
                                            model: model_id.clone(),
                                            choices: vec![ChunkChoice {
                                                index: 0,
                                                delta: ChatMessageDelta {
                                                    role: None,
                                                    content: None,
                                                    reasoning_content: None,
                                                    citations: None,
                                                    tool_calls: Some(vec![tc]),
                                                },
                                                finish_reason: None,
                                            }],
                                            session_url: session_url.clone(),
                                        }));
                                    }
                                }

                                let role = if !emitted_role && !clean_delta.is_empty() {
                                    emitted_role = true;
                                    Some("assistant".to_string())
                                } else {
                                    None
                                };

                                build_streaming_chunk(
                                    &tx,
                                    &counter,
                                    &id_prefix,
                                    &model_id,
                                    clean_delta,
                                    think_delta,
                                    parsed_tool_calls,
                                    citations,
                                    role,
                                    &mut previous_text,
                                    &mut collected_tool_calls,
                                    session_url.as_deref(),
                                );
                            }
                            None => {
                                // Line could be [DONE] or non-data — ignore silently
                            }
                        }
                    }
                }

                buffer.drain(0..consumed);
            }

            // Post-stream tool-call handling. Most markers are stripped and
            // emitted as structured chunks inline during streaming; this block
            // is a fallback that recovers markers the stripper could not
            // complete (truncated closing tag) and ensures the finish_reason
            // reflects any collected tool calls.
            if had_tools {
                mtp_state.finish(&tool_defs_stream);
                for tc in std::mem::take(&mut mtp_state.collected_tool_calls) {
                    if !collected_tool_calls.iter().any(|c| c.id == tc.id) {
                        collected_tool_calls.push(tc.clone());
                        let idx = counter.fetch_add(1, Ordering::Relaxed);
                        let _ = tx.send(Ok(ChatCompletionChunk {
                            id: format!("{}-{}", id_prefix, idx),
                            object: "chat.completion.chunk".to_string(),
                            created: current_timestamp(),
                            model: model_id.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChatMessageDelta {
                                    role: None,
                                    content: None,
                                    reasoning_content: None,
                                    citations: None,
                                    tool_calls: Some(vec![tc]),
                                },
                                finish_reason: None,
                            }],
                            session_url: session_url.clone(),
                        }));
                    }
                }
            }

            let mut finish_reason = if collected_tool_calls.is_empty() {
                "stop".to_string()
            } else {
                "tool_calls".to_string()
            };

            if had_tools && !previous_text.is_empty() {
                // Recover any truncated MTP block the inline stripper could
                // not complete.
                mtp_state.finish(&tool_defs_stream);
                let parsed_opt = std::mem::take(&mut mtp_state.collected_tool_calls);
                let clean_text = previous_text.clone();
                let parsed = if parsed_opt.is_empty() { None } else { Some(parsed_opt) };
                if let Some(calls) = parsed {
                    if !calls.is_empty() {
                        // Emit the tool calls as a single structured
                        // chunk. We append to `collected_tool_calls` so
                        // the storage block below persists them via
                        // `SessionStore::store_tool_calls`, mirroring
                        // the agent-loop semantics of DeepSeek/Gemini.
                        counter.fetch_add(1, Ordering::Relaxed);
                        let chunk = ChatCompletionChunk {
                            id: format!("{}-{}", id_prefix, counter.load(Ordering::Relaxed)),
                            object: "chat.completion.chunk".to_string(),
                            created: current_timestamp(),
                            model: model_id.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChatMessageDelta {
                                    role: None,
                                    content: None,
                                    reasoning_content: None,
                                    citations: None,
                                    tool_calls: Some(calls.clone()),
                                },
                                finish_reason: None,
                            }],
                            session_url: session_url.clone(),
                        };
                        if tx.send(Ok(chunk)).is_err() {
                            return;
                        }

                        for call in &calls {
                            if !collected_tool_calls.iter().any(|c| c.id == call.id) {
                                collected_tool_calls.push(call.clone());
                            }
                        }
                        finish_reason = "tool_calls".to_string();

                        // Discard the now-redundant tool-call marker
                        // text from any subsequent chunks if the page
                        // is still streaming. The page already wrote
                        // it into the DOM, but downstream consumers
                        // see only the cleaned form via `finish_reason`.
                        let _ = clean_text;
                    }
                }
            }

            // Store conversation state for multi-turn
            if let (Some(conv_id), Some(msg_id)) = (&stored_conv_id, &stored_msg_id) {
                let stored = StoredConversation {
                    conversation_id: conv_id.clone(),
                    message_id: msg_id.clone(),
                    model_id: model_id.clone(),
                };
                // A branch token was already minted mid-stream; reuse it so
                // the emitted session_url and the stored tip match. If the
                // stream never surfaced a conversation_id (edge case), mint a
                // fresh one here so every completion still ends up on a
                // branch token of its own.
                let token = conv_token
                    .clone()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(new_session_token);
                store.insert(token.clone(), &stored, &model_id).await;

                if !collected_tool_calls.is_empty() {
                    store
                        .store_tool_calls(&token, &collected_tool_calls)
                        .await;
                }

                if session_url.is_none() {
                    session_url = Some(make_session_url(&token, conv_id));
                }
            }

            // Final chunk with finish_reason (stop or tool_calls)
            counter.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(Ok(ChatCompletionChunk {
                id: format!("{}-final", id_prefix),
                object: "chat.completion.chunk".to_string(),
                created: current_timestamp(),
                model: model_id,
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChatMessageDelta::default(),
                    finish_reason: Some(finish_reason),
                }],
                session_url,
            }));
        });

        Ok(UnboundedReceiverStream::new(rx).boxed())
    }
}

/// Build and emit a single streaming chunk.
#[allow(clippy::too_many_arguments)]
fn build_streaming_chunk(
    tx: &mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
    counter: &AtomicU32,
    id_prefix: &str,
    model_id: &str,
    delta: String,
    thinking: Option<String>,
    tool_calls_raw: Option<Vec<ToolCall>>,
    citations: Option<Vec<Citation>>,
    role: Option<String>,
    previous_text: &mut String,
    collected_tool_calls: &mut Vec<ToolCall>,
    session_url: Option<&str>,
) -> bool {
    if delta.is_empty() && tool_calls_raw.is_none() && citations.is_none() && thinking.is_none() && role.is_none() {
        return true;
    }

    counter.fetch_add(1, Ordering::Relaxed);

    let has_tool_calls = tool_calls_raw.is_some();
    let new_tool_calls = tool_calls_raw.map(|calls| {
        let mut new_calls: Vec<ToolCall> = Vec::new();
        for call in calls {
            if !collected_tool_calls.iter().any(|c| c.id == call.id) {
                new_calls.push(call.clone());
                collected_tool_calls.push(call);
            }
        }
        new_calls
    }).filter(|c: &Vec<ToolCall>| !c.is_empty());

    let clean_delta = if has_tool_calls || !delta.is_empty() {
        delta.clone()
    } else {
        String::new()
    };

    let chunk = ChatCompletionChunk {
        id: format!("{}-{}", id_prefix, counter.load(Ordering::Relaxed)),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model_id.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta {
                role,
                content: if !clean_delta.is_empty() {
                    Some(clean_delta)
                } else if citations.is_some() || new_tool_calls.is_some() {
                    None
                } else {
                    None
                },
                reasoning_content: thinking,
                citations,
                tool_calls: new_tool_calls,
            },
            finish_reason: None,
        }],
        session_url: session_url.map(|s| s.to_string()),
    };

    if !delta.is_empty() {
        previous_text.push_str(&delta);
    }

    tx.send(Ok(chunk)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::chatgpt::state::SessionStore;
    use reqwest::Client;

    fn test_client(store: SessionStore) -> ChatGptDirectClient {
        ChatGptDirectClient {
            http: Client::new(),
            auth: AuthData {
                access_token: String::new(),
                user_agent: String::new(),
            },
            store,
            model_id: "chatgpt-auto".to_string(),
            internal_model_id: "text-davinci-002-render-sha".to_string(),
            session_token: None,
            thinking_cache: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn store_conversation_mints_fresh_branch_token() {
        let store = SessionStore::new();
        let client = test_client(store.clone());
        let conv = StoredConversation {
            conversation_id: "conv-1".to_string(),
            message_id: "msg-1".to_string(),
            model_id: "chatgpt-auto".to_string(),
        };

        // Even when the caller passes an existing continuation token, the
        // stored turn must live under a brand-new token so that a second
        // continuation started from the same parent forks into its own
        // branch instead of overwriting the shared tip.
        let url = client
            .store_conversation(&conv, Some("chatgpt_existing-token"))
            .await;
        let minted = extract_session_token(&url).expect("session_url must carry a token");
        assert!(minted.starts_with("chatgpt_"), "token should be fresh: {minted}");
        assert_ne!(minted, "chatgpt_existing-token");

        let resolved = store.acquire(&minted).await.expect("branch must be resolvable");
        assert_eq!(resolved.conversation_id, "conv-1");
        assert_eq!(resolved.message_id, "msg-1");
    }

    #[tokio::test]
    async fn two_continuations_from_same_parent_fork_into_independent_branches() {
        let store = SessionStore::new();
        let client = test_client(store.clone());
        let conv = StoredConversation {
            conversation_id: "conv-1".to_string(),
            message_id: "msg-1".to_string(),
            model_id: "chatgpt-auto".to_string(),
        };

        // Two concurrent requests that both started from the same parent
        // must not share a stored tip: each mints its own branch token and
        // resolves back to the same conversation with its own message_id.
        let url_a = client.store_conversation(&conv, None).await;
        let token_a = extract_session_token(&url_a).unwrap();
        let conv_b = StoredConversation {
            conversation_id: "conv-1".to_string(),
            message_id: "msg-2a".to_string(),
            model_id: "chatgpt-auto".to_string(),
        };
        let url_b = client.store_conversation(&conv_b, None).await;
        let token_b = extract_session_token(&url_b).unwrap();

        assert_ne!(token_a, token_b);
        assert_eq!(store.acquire(&token_a).await.unwrap().message_id, "msg-1");
        assert_eq!(store.acquire(&token_b).await.unwrap().message_id, "msg-2a");
    }
}
