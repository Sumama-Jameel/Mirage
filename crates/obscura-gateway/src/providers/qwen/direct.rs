use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::{BoxStream, StreamExt};
use obscura_net::StealthHttpClient;
use tokio::sync::mpsc;

use crate::error::GatewayError;

use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatContent,
    ChatMessage, ChatMessageDelta, ChunkChoice, ToolCall, Usage,
};
use crate::providers::streaming_upload::download_and_hash_batch;
use crate::session::{SessionHandle, SessionManager};

use super::auth::resolve_token;
use super::state::QwenSessionStore;

const BASE_URL: &str = "https://chat.qwen.ai";
const API_PATH: &str = "/api/v2";

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}


fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Map public model IDs to Qwen's upstream model name.
fn upstream_model(model_id: &str) -> String {
    match model_id {
        "qwen-plus" | "qwen-auto" => "qwen3.7-plus".to_string(),
        "qwen-max" => "qwen3.7-plus".to_string(),
        "qwen-flash" => "qwen3.5-flash".to_string(),
        "qwen-coder" => "qwen3-coder-plus".to_string(),
        "qwen-vl" => "qwen3-vl-plus".to_string(),
        "qwen3.8-max"
        | "qwen3.8-max-preview"
        | "qwen3.7-plus"
        | "qwen3.6-max-preview"
        | "qwen3.5-omni-plus"
        | "qwen3.6-plus"
        | "qwen3.7-max"
        | "qwen3.7"
        | "qwen3.6"
        | "qwen3.5"
        | "qwen3"
        | "qwen3-vl"
        | "qwen3-coder"
        | "qwen3-vl-235b-a22b" => model_id.to_string(),
        _ => {
            // Pass through unknown model IDs so we can probe the API directly
            model_id.to_string()
        }
    }
}

/// A single Qwen SSE event.
#[derive(Debug, Clone, serde::Deserialize)]
struct QwenSSEChoice {
    delta: QwenSSEDelta,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    index: i32,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct QwenThinkingExtra {
    #[serde(default)]
    summary_title: Option<QwenExtraContent>,
    #[serde(default)]
    summary_thought: Option<QwenExtraContent>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct QwenExtraContent {
    #[serde(default)]
    content: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct QwenToolCallDelta {
    #[serde(default)]
    index: Option<i32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    r#type: Option<String>,
    #[serde(default)]
    function: Option<QwenFunctionDelta>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct QwenFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct QwenSSEDelta {
    #[serde(default)]
    #[allow(dead_code)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    extra: Option<QwenThinkingExtra>,
    #[serde(default)]
    tool_calls: Option<Vec<QwenToolCallDelta>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct QwenSSEEvent {
    #[serde(default)]
    choices: Vec<QwenSSEChoice>,
    /// `response.created` carries the id of the assistant message being
    /// streamed. The next turn must link to it via `parent_id`.
    #[serde(rename = "response.created", default)]
    response_created: Option<QwenResponseCreated>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct QwenResponseCreated {
    #[serde(default)]
    parent_id: Option<String>,
}

/// A single direct-API request context for Qwen.
pub struct DirectClient {
    stealth: Arc<StealthHttpClient>,
    session: SessionHandle,
    model_id: String,
    request_id: String,
    store: QwenSessionStore,
    upload_cache: super::upload::UploadCache,
}

/// Set the `user-agent` header for the stealth client so it matches
/// what the browser would send. All per-request headers (Accept, Content-Type,
/// Version, source, X-Request-Id, X-Accel-Buffering, Referer) are set by
/// `build_request_headers` to avoid duplicates.
fn build_extra_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("user-agent".to_string(), "Mozilla/5.0 (X11; Linux x86_64; rv:140.12) Gecko/20100101 Firefox/140.12".to_string());
    h
}

/// Per-request headers matching the web app JS interceptor exactly.
/// The web app deletes `authorization`, `origin`, `referer`, `sec-fetch-*`,
/// and `accept-language` from incoming headers, then sets these explicitly.
/// X-Request-Id is fixed per session so the API can correlate requests.
fn build_request_headers(request_id: &str) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("content-type".to_string(), "application/json".to_string());
    h.insert("accept".to_string(), "application/json, text/plain, */*".to_string());
    h.insert("version".to_string(), "0.2.73".to_string());
    h.insert("source".to_string(), "web".to_string());
    h.insert("x-request-id".to_string(), request_id.to_string());
    h.insert("x-accel-buffering".to_string(), "no".to_string());
    h.insert("referer".to_string(), "https://chat.qwen.ai/".to_string());
    h.insert("origin".to_string(), "https://chat.qwen.ai".to_string());
    h
}

/// Send a request via StealthHttpClient with simple retry logic.
async fn send_stealth(
    stealth: &StealthHttpClient,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: &str,
) -> Result<obscura_net::Response, GatewayError> {
    let parsed_url = url::Url::parse(url)
        .map_err(|e| GatewayError::Internal(format!("invalid URL {url}: {e}")))?;

    for attempt in 1..=3 {
        match stealth.send_single(method, &parsed_url, headers, body).await {
            Ok(resp) => {
                let code = resp.status;
                if code >= 500 || code == 429 {
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
                        continue;
                    }
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
                    continue;
                }
                return Err(GatewayError::Internal(format!("stealth request failed: {e}")));
            }
        }
    }
    Err(GatewayError::Internal("stealth request exhausted retries".to_string()))
}

impl DirectClient {
    pub async fn new(
        session: SessionHandle,
        _sessions: &SessionManager,
        model_id: &str,
        store: QwenSessionStore,
    ) -> Result<Self, GatewayError> {
        // Reuse the imported cookie jar so Qwen cookies (token, cna, acw_tc, etc.)
        // from Firefox are available for all stealth requests.
        let stealth = Arc::new(StealthHttpClient::new(session.cookie_jar.clone()));

        let extra = build_extra_headers();
        stealth.set_extra_headers(extra).await;

        // Generate a single X-Request-Id for the entire session lifecycle.
        // The Qwen API echoes it back in session creation; the completion may
        // require the same ID to correlate with the session.
        let request_id = new_uuid();
        tracing::info!(request_id = %request_id, "Qwen DirectClient created");

        Ok(Self {
            stealth,
            session,
            model_id: model_id.to_string(),
            request_id,
            store,
            upload_cache: super::upload::UploadCache::new(),
        })
    }

    /// Create a new upstream chat session.
    /// Returns (id, parent_id, chat_id) matching qwen2api-rs fields.
    async fn create_chat_session(&self, has_vision: bool) -> Result<(String, String, String), GatewayError> {
        // The web app only ever uses `t2t` (even with attached images); `v2t`
        // is not a valid chat_type and yields Bad_Request.
        let _ = has_vision;
        let chat_type = "t2t";
        let payload = serde_json::json!({
            "title": "New Chat",
            "models": [""],
            "chat_mode": "normal",
            "chat_type": chat_type,
            "timestamp": unix_secs(),
        });

        let url = format!("{}{}/chats/new", BASE_URL, API_PATH);
        let body_str = serde_json::to_string(&payload)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;

        let response = send_stealth(
            self.stealth.as_ref(),
            "POST",
            &url,
            &build_request_headers(&self.request_id),
            &body_str,
        )
        .await?;

        let status = response.status;
        let body = String::from_utf8(response.body)
            .unwrap_or_else(|e| format!("<non-utf8 body: {e}>"));

        if status != 200 {
            if status == 401 {
                return Err(GatewayError::Provider(
                    "Qwen JWT rejected (401); log in at chat.qwen.ai and re-import your Firefox profile"
                        .to_string(),
                ));
            }
            if body.trim_start().starts_with("<!") {
                return Err(GatewayError::Provider(
                    "Qwen WAF challenge encountered — session may need refresh or re-import".to_string(),
                ));
            }
            return Err(GatewayError::Provider(format!(
                "Qwen create session failed ({}): {}",
                status, body
            )));
        }

        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| GatewayError::Internal(format!("failed to parse Qwen session response: {e}, body: {body}")))?;

        tracing::info!(body = %body, "Qwen create session response");

        let success = parsed.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
        let id = parsed
            .get("data")
            .and_then(|d| d.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                GatewayError::Internal(format!(
                    "Qwen session response missing 'data.id' field: {body}"
                ))
            })?;
        let parent_id = parsed
            .get("data")
            .and_then(|d| d.get("parentId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "0".to_string());
        let chat_id = parsed
            .get("data")
            .and_then(|d| d.get("chatId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| id.clone());

        if !success {
            return Err(GatewayError::Provider(format!(
                "Qwen session creation returned success=false (id={id}): {body}"
            )));
        }

        tracing::info!(id = %id, parent_id = %parent_id, chat_id = %chat_id, "Qwen chat session created");
        Ok((id, parent_id, chat_id))
    }

    /// Delete an upstream chat session.
    async fn delete_chat_session(chat_id: &str, stealth: &StealthHttpClient) {
        let url = format!("{}{}/chats/{}", BASE_URL, API_PATH, chat_id);
        let parsed_url = match url::Url::parse(&url) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(chat_id = %chat_id, error = %e, "Qwen DELETE invalid URL");
                return;
            }
        };
        match stealth.send_single("DELETE", &parsed_url, &HashMap::new(), "").await {
            Ok(resp) => {
                if resp.status != 200 {
                    tracing::warn!(
                        chat_id = %chat_id,
                        status = resp.status,
                        "Qwen DELETE session returned non-success"
                    );
                } else {
                    tracing::info!(chat_id = %chat_id, "Qwen chat session deleted");
                }
            }
            Err(e) => {
                tracing::warn!(chat_id = %chat_id, error = %e, "Qwen DELETE session failed");
            }
        }
    }

    async fn handle_tool_results(&self, gateway_session_id: &str, messages: &[ChatMessage]) -> String {
        let tool_msgs: Vec<&ChatMessage> = messages.iter().filter(|m| m.role == "tool").collect();
        if tool_msgs.is_empty() {
            return String::new();
        }

        let mut items = Vec::new();
        for msg in tool_msgs {
            let call = match msg.tool_call_id.as_deref() {
                Some(id) => self.store.get_tool_call(gateway_session_id, id).await,
                None => None,
            };
            items.push((call, msg.tool_call_id.clone(), msg.content.as_text()));
        }

        let refs: Vec<(Option<&crate::models::ToolCall>, Option<&str>, &str)> = items
            .iter()
            .map(|(c, id, o)| (c.as_ref(), id.as_deref(), o.as_str()))
            .collect();

        crate::providers::mtp::format_tool_results(&refs)
    }

    /// Build the messages payload body matching the web app exact format.
    ///
    /// `last_message_id` is the id of the previous assistant message (from the
    /// prior turn's `response.created` SSE event). The new user message links
    /// to it via `parent_id` so the server reconstructs conversation context;
    /// null on the first turn of a chat.
    async fn build_completion_payload(&self, request: &ChatCompletionRequest, chat_id: &str, last_message_id: Option<&str>, stream: bool, file_objects: &[serde_json::Value], has_vision: bool, gateway_session_id: &str) -> serde_json::Value {
        let _ = has_vision;
        let ts = unix_secs();
        let fid = new_uuid();
        let child_id = new_uuid();
        let model = upstream_model(&self.model_id);
        // Fold all messages EXCEPT the newest user instruction into a
        // transcript; the instruction is rendered last so it cannot be
        // buried under transcript volume (the failure mode that made qwen
        // echo text instead of emitting tool blocks).
        let last_user_idx = request
            .messages
            .iter()
            .rposition(|m| m.role == "user");
        let current_request = last_user_idx
            .and_then(|i| request.messages.get(i))
            .map(|m| m.content.as_text())
            .unwrap_or_default();
        let mut transcript_parts: Vec<String> = Vec::new();
        for m in request.messages.iter() {
            let text = m.content.as_text();
            if text.is_empty() {
                continue;
            }
            let label = match m.role.as_str() {
                "system" => "System",
                "assistant" => "Assistant",
                "user" => "Human",
                other => {
                    // Tool-role turns are rendered as MTP result blocks in
                    // the tool_results slot below, not as transcript lines.
                    if other == "tool" {
                        continue;
                    }
                    other
                }
            };
            transcript_parts.push(format!("{}: {}", label, text));
        }
        let transcript = transcript_parts.join("\n");

        let tool_context = self.handle_tool_results(gateway_session_id, &request.messages).await;

        let continuation = crate::providers::mtp::has_mirage_history(&transcript) || !tool_context.is_empty();
        let system_prompt = if continuation { None } else {
            request.tools.as_ref().filter(|t| !t.is_empty()).map(|tools| {
                crate::providers::mtp::build_mtp_system_prompt(
                    tools,
                    request.tool_choice.as_ref(),
                    false,
                    crate::providers::mtp::prompt_style_for_model(&request.model)
                )
            })
        };

        let content = crate::providers::mtp::compose_flat_prompt(crate::providers::mtp::FlatPrompt {
            system: system_prompt.as_deref(),
            transcript: &transcript,
            tool_results: &tool_context,
            current_request: &current_request,
        });

        let files = file_objects.to_vec();

        // The web app only uses `t2t` (even with attached images).
        let chat_type = "t2t";

        // Message-tree linkage: the new user message becomes a child of the
        // previous assistant message (its id came back in `response.created`).
        // First turn of a chat: `parentId` is empty and `parent_id` null.
        let top_parent_id = match last_message_id {
            Some(id) => serde_json::Value::String(id.to_string()),
            None => serde_json::Value::String(String::new()),
        };
        let null_parent_id = match last_message_id {
            Some(id) => serde_json::Value::String(id.to_string()),
            None => serde_json::Value::Null,
        };
        let parent_id = last_message_id.unwrap_or("");

        let payload = serde_json::json!({
            "stream": stream,
            "version": "2.1",
            "incremental_output": true,
            "chatId": chat_id,
            "parentId": top_parent_id,
            "chat_id": chat_id,
            "chat_mode": "normal",
            "model": model,
            "parent_id": null_parent_id,
            "messages": [{
                "id": null,
                "fid": fid,
                "parentId": parent_id,
                "childrenIds": [child_id],
                "role": "user",
                "content": content,
                "user_action": "chat",
                "files": files,
                "timestamp": ts,
                "models": [model],
                "model": "",
                "chat_type": chat_type,
                "feature_config": {
                    "thinking_enabled": request.thinking.unwrap_or(true),
                    "output_schema": "phase",
                    "research_mode": "normal",
                    "auto_thinking": true,
                    "thinking_mode": "Auto",
                    "thinking_format": "summary",
                    "auto_search": request.search.unwrap_or(false)
                },
                "extra": {
                    "meta": {
                        "subChatType": chat_type
                    }
                },
                "sub_chat_type": chat_type,
                "parent_id": null_parent_id
            }],
            "timestamp": ts
        });

        // DO NOT send a `tools` array in the payload. Qwen's server validates
        // tool names against its own built-in registry (code_interpreter,
        // web_search, web_extractor) and responds with "Tool X does not exists"
        // for any custom tool name. Instead, tools are injected as XML text
        // via inject_tool_prompt above, and the model emits <tool_call>
        // XML markers in the content which we parse below.

        payload
    }

    async fn process_attachments(&self, request: &ChatCompletionRequest) -> Result<(Vec<serde_json::Value>, bool), GatewayError> {
        let image_urls: Vec<String> = request.messages.iter()
            .flat_map(|m| m.content.image_urls())
            .collect();
        let file_urls: Vec<String> = request.messages.iter()
            .flat_map(|m| m.content.file_urls())
            .collect();
        let all_urls: Vec<String> = image_urls.iter().chain(file_urls.iter()).cloned().collect();

        let has_vision = !image_urls.is_empty();

        if all_urls.is_empty() {
            return Ok((Vec::new(), has_vision));
        }

        // Resolve JWT token; if missing, uploads will fail but chat may still work.
        let token = resolve_token(&self.session.local_storage).ok();

        // Build http client for streaming downloads
        let http = reqwest::Client::new();
        
        // Separate data URIs from remote URLs
        let mut data_uris = Vec::new();
        let mut remote_urls = Vec::new();
        
        for url in &all_urls {
            if url.starts_with("data:") {
                data_uris.push(url.clone());
            } else {
                remote_urls.push(url.clone());
            }
        }

        let mut processed = Vec::new();

        // Concurrently download and hash remote URLs
        if !remote_urls.is_empty() {
            match download_and_hash_batch(&http, remote_urls).await {
                Ok(hashed_files) => {
                    for hashed in hashed_files {
                        match super::upload::upload_hashed_file(
                            &self.stealth,
                            &self.request_id,
                            token.as_deref(),
                            &self.upload_cache,
                            &hashed.bytes,
                            &hashed.name,
                            &hashed.mime_type,
                        ).await {
                            Ok(file_obj) => processed.push(file_obj),
                            Err(e) => {
                                tracing::warn!(error = %e, "Qwen concurrent file upload failed, skipping");
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Qwen concurrent download failed");
                }
            }
        }

        // Process data URIs sequentially
        for url in data_uris {
            match super::upload::resolve_url(&url, &self.stealth, &self.request_id, token.as_deref(), &self.upload_cache).await {
                Ok(file_obj) => processed.push(file_obj),
                Err(e) => {
                    tracing::warn!(url = %url, error = %e, "Qwen data URI upload failed, skipping attachment");
                }
            }
        }

        // If all attachments failed and this was a vision request, surface the error.
        if has_vision && processed.is_empty() && !all_urls.is_empty() {
            return Err(GatewayError::Provider(
                "All file uploads failed. Ensure you're logged in at chat.qwen.ai and have re-imported your Firefox profile (the JWT token is missing or expired)."
                    .to_string(),
            ));
        }

        Ok((processed, has_vision))
    }

    /// Extract thinking text from the extra.summary_title / summary_thought.
    fn extract_thinking_text(extra: &Option<QwenThinkingExtra>) -> String {
        let mut text = String::new();
        if let Some(e) = extra {
            if let Some(t) = &e.summary_title {
                for line in &t.content {
                    text.push_str(line);
                    text.push('\n');
                }
            }
            if let Some(t) = &e.summary_thought {
                for line in &t.content {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
        text
    }

    /// Parse SSE body into accumulated text, reasoning, native tool calls,
    /// and the id of the created assistant message (from `response.created`).
    ///
    /// Native tool calls arrive as OpenAI-style `delta.tool_calls` objects.
    /// Arguments may be split across multiple deltas, so they are accumulated
    /// by call index and merged into complete `ToolCall` objects.
    fn parse_sse_events(body: &str) -> (String, String, String, Vec<ToolCall>, Option<String>) {
        let mut full_text = String::new();
        let mut reasoning_text = String::new();
        let mut finish_reason = "stop".to_string();
        let mut native_calls: Vec<ToolCall> = Vec::new();
        let mut created_message_id: Option<String> = None;

        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line == "data: [DONE]" {
                continue;
            }
            let json_str = match line.strip_prefix("data: ") {
                Some(s) => s,
                None => continue,
            };
            let event: QwenSSEEvent = match serde_json::from_str(json_str) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if let Some(created) = &event.response_created {
                if created_message_id.is_none() {
                    created_message_id = created.parent_id.clone();
                }
            }
            for choice in event.choices {
                if let Some(ref fr) = choice.finish_reason {
                    finish_reason = fr.clone();
                }

                // Collect native tool call deltas.
                if let Some(ref deltas) = choice.delta.tool_calls {
                    for delta in deltas {
                        let idx = delta.index.unwrap_or(0) as usize;
                        while native_calls.len() <= idx {
                            native_calls.push(ToolCall {
                                id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                                r#type: "function".to_string(),
                                function: crate::models::FunctionCall {
                                    name: String::new(),
                                    arguments: String::new(),
                                },
                            });
                        }
                        if let Some(ref id) = delta.id {
                            native_calls[idx].id = id.clone();
                        }
                        if let Some(ref f) = delta.function {
                            if let Some(ref name) = f.name {
                                native_calls[idx].function.name = name.clone();
                            }
                            if let Some(ref args) = f.arguments {
                                native_calls[idx].function.arguments.push_str(args);
                            }
                        }
                    }
                }

                let content = choice.delta.content.unwrap_or_default();
                let phase = choice.delta.phase.as_deref();

                // If content is empty but this is a thinking_summary event,
                // extract from the `extra` field instead.
                if content.is_empty()
                    && phase == Some("thinking_summary")
                    && choice.delta.extra.is_some()
                {
                    let t = Self::extract_thinking_text(&choice.delta.extra);
                    if !t.is_empty() {
                        reasoning_text.push_str(&t);
                    }
                    continue;
                }

                if content.is_empty() {
                    continue;
                }
                match phase {
                    Some("think") | Some("thinking_summary") => {
                        reasoning_text.push_str(&content);
                    }
                    _ => {
                        full_text.push_str(&content);
                    }
                }
            }
        }

        // Drop empty placeholder tool calls (no name/arguments were ever set).
        native_calls.retain(|c| !c.function.name.is_empty());

        (full_text, reasoning_text, finish_reason, native_calls, created_message_id)
    }

    /// Non-streaming chat completion. Accumulates SSE deltas in-memory.
    pub async fn chat(
        &self,
        request: ChatCompletionRequest,
        gateway_session_id: &str,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let response_id = format!("chatcmpl-{}", &new_uuid()[..8]);

        // Step 0: process file attachments
        let (file_urls, has_vision) = self.process_attachments(&request).await?;

        // Step 1: get or create session
        let (session_id, _parent_id, chat_id, last_message_id) =
            if let Some(existing) = self.store.get(gateway_session_id).await {
                (
                    existing.chat_id.clone(),
                    "0".to_string(),
                    existing.chat_id,
                    existing.last_message_id.clone(),
                )
            } else {
                let (sid, pid, cid) = self.create_chat_session(has_vision).await?;
                self.store.insert(gateway_session_id.to_string(), super::state::QwenSessionState {
                    chat_id: cid.clone(),
                    model: self.model_id.clone(),
                    tool_calls: HashMap::new(),
                    last_message_id: None,
                }).await;
                (sid, pid, cid, None)
            };

        // Step 2: stream and accumulate
        let payload = self.build_completion_payload(&request, &chat_id, last_message_id.as_deref(), true, &file_urls, has_vision, gateway_session_id).await;
        let url = format!(
            "{}{}/chat/completions?chat_id={}",
            BASE_URL, API_PATH, session_id,
        );

        let body_str = serde_json::to_string(&payload)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;

        tracing::info!(payload = %body_str, url = %url, headers = ?build_request_headers(&self.request_id), "Qwen completion request");

        let response = send_stealth(
            self.stealth.as_ref(),
            "POST",
            &url,
            &build_request_headers(&self.request_id),
            &body_str,
        )
        .await?;

        let status = response.status;
        let body = String::from_utf8(response.body)
            .unwrap_or_else(|e| format!("<non-utf8 body: {e}>"));

        tracing::info!(
            status = status,
            body_len = body.len(),
            "Qwen completion raw response"
        );

        if body.trim_start().starts_with("<!") {
            return Err(GatewayError::Provider(
                "Qwen WAF challenge encountered".to_string(),
            ));
        }
        if status != 200 || body.contains("\"success\":false") {
            tracing::error!(
                status = status,
                body = %body,
                "Qwen completion failed"
            );
            return Err(GatewayError::Provider(format!(
                "Qwen completion failed ({}): {}",
                status, body
            )));
        }

        tracing::info!(body_len = body.len(), "Qwen completion response has content");
        let (raw_text, reasoning_text, mut finish_reason, native_calls, created_message_id) =
            Self::parse_sse_events(&body);

        // Record the created assistant message id so the next turn links to it.
        if let Some(msg_id) = &created_message_id {
            self.store.store_last_message_id(gateway_session_id, msg_id).await;
        }

        let has_tools = request.tools.is_some();
        // Native tool_calls from the SSE delta take precedence; otherwise
        // parse MTP blocks from the content text (the server rejects custom
        // tools in the native payload, so the model emits MTP markers).
        let (full_text, tool_calls) = if has_tools && !native_calls.is_empty() {
            (raw_text, Some(native_calls))
        } else if has_tools {
            let defs: Vec<crate::models::Tool> = request.tools.clone().unwrap_or_default();
            let mut st = crate::providers::mtp::MtpStreamState::new();
            let stripped = st.process_delta(&raw_text, &defs);
            st.finish(&defs);
            if !st.collected_tool_calls.is_empty() {
                (stripped, Some(std::mem::take(&mut st.collected_tool_calls)))
            } else {
                (raw_text, None)
            }
        } else {
            (raw_text, None)
        };

        if let Some(ref calls) = tool_calls {
            finish_reason = "tool_calls".to_string();
            self.store.store_tool_calls(gateway_session_id, calls).await;
        }

        let prompt_text: String = request
            .messages
            .iter()
            .map(|m| m.content.as_text())
            .collect();
        let usage = estimate_cost(&prompt_text, &full_text);

        Ok(ChatCompletionResponse {
            id: response_id,
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: request.model.clone(),
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: ChatContent::String(full_text),
                    name: None,
                    reasoning_content: if reasoning_text.is_empty() {
                        None
                    } else {
                        Some(reasoning_text)
                    },
                    citations: None,
                    tool_calls: tool_calls.clone(),
                    tool_call_id: None,
                },
                finish_reason,
            }],
            usage,
            session_url: Some(format!("{}/c/{}", BASE_URL, chat_id)),
        })
    }

    /// Streaming chat completion. Forwards each SSE delta as a chunk.
    pub async fn chat_stream(
        &self,
        request: ChatCompletionRequest,
        gateway_session_id: &str,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError>
    {
        let response_id = format!("chatcmpl-{}", &new_uuid()[..8]);
        let model_id = request.model.clone();

        // Step 0: process file attachments
        let (file_urls, has_vision) = self.process_attachments(&request).await?;

        // Step 1: get or create session
        let (session_id, _parent_id, chat_id, last_message_id) =
            if let Some(existing) = self.store.get(gateway_session_id).await {
                (
                    existing.chat_id.clone(),
                    "0".to_string(),
                    existing.chat_id,
                    existing.last_message_id.clone(),
                )
            } else {
                let (sid, pid, cid) = self.create_chat_session(has_vision).await?;
                self.store.insert(gateway_session_id.to_string(), super::state::QwenSessionState {
                    chat_id: cid.clone(),
                    model: self.model_id.clone(),
                    tool_calls: HashMap::new(),
                    last_message_id: None,
                }).await;
                (sid, pid, cid, None)
            };

        let payload = self.build_completion_payload(&request, &chat_id, last_message_id.as_deref(), request.stream, &file_urls, has_vision, gateway_session_id).await;
        let url = format!(
            "{}{}/chat/completions?chat_id={}",
            BASE_URL, API_PATH, session_id
        );

        let body_str = serde_json::to_string(&payload)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;

        let comp_headers = build_request_headers(&self.request_id);

        tracing::info!(payload = %body_str, url = %url, headers = ?comp_headers, "Qwen stream completion request");

        // Use reqwest directly for true HTTP streaming. We copy cookies and
        // extra headers from the stealth client to maintain the same session.
        let parsed_url = url::Url::parse(&url)
            .map_err(|e| GatewayError::Internal(format!("invalid URL: {e}")))?;
        let cookie_header = self.stealth.cookie_header_for_url(&parsed_url);
        let extra_headers = self.stealth.get_extra_headers().await;

        let streaming_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build streaming client: {e}")))?;

        let mut req_builder = streaming_client
            .post(&url)
            .body(body_str.clone());
        if !cookie_header.is_empty() {
            req_builder = req_builder.header("cookie", &cookie_header);
        }
        for (k, v) in &extra_headers {
            req_builder = req_builder.header(k.as_str(), v.as_str());
        }
        for (k, v) in &comp_headers {
            req_builder = req_builder.header(k.as_str(), v.as_str());
        }

        let resp = req_builder.send().await
            .map_err(|e| GatewayError::Provider(format!("Qwen streaming request failed: {e}")))?;

        let status = resp.status();
        if status != 200 {
            let err_body = resp.text().await.unwrap_or_default();
            // A failed completion invalidates the stored chat reference. Drop
            // the store entry and delete the upstream chat so a retry (or the
            // next turn) creates a fresh chat instead of reusing a broken id.
            self.store.remove(gateway_session_id).await;
            let _ = Self::delete_chat_session(&session_id, self.stealth.as_ref()).await;
            if err_body.trim_start().starts_with("<!") {
                return Err(GatewayError::Provider(
                    "Qwen WAF challenge encountered".to_string(),
                ));
            }
            return Err(GatewayError::Provider(format!(
                "Qwen completion failed ({}): {}",
                status,
                err_body.chars().take(300).collect::<String>()
            )));
        }

        let (tx, rx) = mpsc::channel(256);
        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let session_url = Some(format!("{}/c/{}", BASE_URL, chat_id));

        let has_tools = request.tools.is_some();
        let tool_defs: Vec<crate::models::Tool> = request.tools.clone().unwrap_or_default();
        let byte_stream = resp.bytes_stream();

        // Spawn a task to process the SSE stream incrementally.
        let store = self.store.clone();
        let gateway_session_id_owned = gateway_session_id.to_string();
        tokio::spawn(async move {
            let mut sent_first_chunk = false;
            let mut full_content_buf = String::new();
            let mut sent_tool_calls = false;
            let mut line_buf = String::new();
            // MTP state: absorbs [MIRAGE_TOOL_CALL_V1] blocks from content
            // deltas so they never leak to the client, and collects calls.
            let mut mtp_state = crate::providers::mtp::MtpStreamState::new();
            // Per-call-id argument and name accumulation for native tool call
            // deltas split across multiple SSE frames.
            let mut native_arg_bufs: HashMap<String, String> = HashMap::new();
            let mut native_arg_names: HashMap<String, String> = HashMap::new();
            let mut native_calls_list: Vec<ToolCall> = Vec::new();

            use futures::pin_mut;
            pin_mut!(byte_stream);

            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.try_send(Err(GatewayError::Provider(format!(
                            "Qwen stream read error: {e}"
                        ))));
                        return;
                    }
                };

                // Append chunk bytes to the line buffer, splitting on newlines
                let chunk_str = String::from_utf8_lossy(&chunk);
                for ch in chunk_str.chars() {
                    if ch == '\n' {
                        let line = std::mem::take(&mut line_buf);
                        let line = line.trim().to_string();

                        if line.is_empty() || line == "data: [DONE]" {
                            continue;
                        }

                        let json_str = match line.strip_prefix("data: ") {
                            Some(s) => s,
                            None => continue,
                        };

                        let event: QwenSSEEvent = match serde_json::from_str(json_str) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };

                        // Capture the created assistant message id from the
                        // first `response.created` event so the next turn can
                        // link to it via parent_id.
                        if let Some(created) = &event.response_created {
                            if let Some(parent_id) = &created.parent_id {
                                store.store_last_message_id(&gateway_session_id_owned, parent_id).await;
                            }
                        }

                        for choice in event.choices {
                            if sent_tool_calls {
                                continue;
                            }

                            if let Some(ref fr) = choice.finish_reason {
                                let _ = tx.try_send(Ok(ChatCompletionChunk {
                                    id: response_id.clone(),
                                    object: "chat.completion.chunk".to_string(),
                                    created: current_timestamp(),
                                    model: model_id.clone(),
                                    choices: vec![ChunkChoice {
                                        index: choice.index,
                                        delta: ChatMessageDelta::default(),
                                        finish_reason: if has_tools { None } else { Some(fr.clone()) },
                                    }],
                                    session_url: session_url.clone(),
                                }));
                                continue;
                            }

                            // Native tool call deltas: forward them as
                            // structured `tool_calls` chunks immediately.
                            if has_tools {
                                if let Some(ref deltas) = choice.delta.tool_calls {
                                    for delta in deltas {
                                        let call_id = delta.id.clone().unwrap_or_else(|| {
                                            format!("call_{}", uuid::Uuid::new_v4().simple())
                                        });
                                        let delta_name = delta.function.as_ref()
                                            .and_then(|f| f.name.clone())
                                            .unwrap_or_default();
                                        let delta_args = delta.function.as_ref()
                                            .and_then(|f| f.arguments.clone())
                                            .unwrap_or_default();

                                        // First time we see this call id: record
                                        // the name and initialize the arg buffer.
                                        if !native_arg_names.contains_key(&call_id) {
                                            if !delta_name.is_empty() {
                                                native_arg_names.insert(call_id.clone(), delta_name.clone());
                                            }
                                            native_arg_bufs.insert(call_id.clone(), String::new());
                                        }
                                        // Accumulate arguments across split deltas.
                                        if let Some(buf) = native_arg_bufs.get_mut(&call_id) {
                                            buf.push_str(&delta_args);
                                        }

                                        // Only emit a chunk once we know the
                                        // function name (the first delta carries it).
                                        if delta_name.is_empty() {
                                            continue;
                                        }

                                        let full_args = native_arg_bufs
                                            .get(&call_id)
                                            .cloned()
                                            .unwrap_or(delta_args);
                                        let tc = ToolCall {
                                            id: call_id.clone(),
                                            r#type: "function".to_string(),
                                            function: crate::models::FunctionCall {
                                                name: delta_name,
                                                arguments: full_args,
                                            },
                                        };
                                        if !native_calls_list.iter().any(|c| c.id == tc.id) {
                                            native_calls_list.push(tc.clone());
                                        }
                                        sent_tool_calls = true;
                                        store.store_tool_calls(&gateway_session_id_owned, std::slice::from_ref(&tc)).await;
                                        let _ = tx.try_send(Ok(ChatCompletionChunk {
                                            id: response_id.clone(),
                                            object: "chat.completion.chunk".to_string(),
                                            created: current_timestamp(),
                                            model: model_id.clone(),
                                            choices: vec![ChunkChoice {
                                                index: choice.index,
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

                            let content = choice.delta.content.unwrap_or_default();
                            let phase = choice.delta.phase.as_deref();
                            let is_thinking = phase == Some("think") || phase == Some("thinking_summary");

                            if content.is_empty() && is_thinking && choice.delta.extra.is_some() {
                                let t = DirectClient::extract_thinking_text(&choice.delta.extra);
                                if !t.is_empty() {
                                    let _ = tx.try_send(Ok(ChatCompletionChunk {
                                        id: response_id.clone(),
                                        object: "chat.completion.chunk".to_string(),
                                        created: current_timestamp(),
                                        model: model_id.clone(),
                                        choices: vec![ChunkChoice {
                                            index: choice.index,
                                            delta: ChatMessageDelta {
                                                role: None,
                                                content: None,
                                                reasoning_content: Some(t),
                                                citations: None,
                                                tool_calls: None,
                                            },
                                            finish_reason: None,
                                        }],
                                        session_url: session_url.clone(),
                                    }));
                                }
                                continue;
                            }

                            if content.is_empty() {
                                continue;
                            }

                            let role = if !sent_first_chunk {
                                sent_first_chunk = true;
                                Some("assistant".to_string())
                            } else {
                                None
                            };

                            if !is_thinking {
                                full_content_buf.push_str(&content);
                            }

                            // Feed non-thinking content through the MTP
                            // state; blocks are absorbed and validated.
                            let visible = if !is_thinking && has_tools {
                                let visible =
                                    mtp_state.process_delta(&content, &tool_defs);
                                if !mtp_state.collected_tool_calls.is_empty() {
                                    for tc in mtp_state.collected_tool_calls.drain(..) {
                                        if !native_calls_list.iter().any(|c| c.id == tc.id) {
                                            native_calls_list.push(tc.clone());
                                        }
                                        store.store_tool_calls(&gateway_session_id_owned, std::slice::from_ref(&tc)).await;
                                        let _ = tx.try_send(Ok(ChatCompletionChunk {
                                            id: response_id.clone(),
                                            object: "chat.completion.chunk".to_string(),
                                            created: current_timestamp(),
                                            model: model_id.clone(),
                                            choices: vec![ChunkChoice {
                                                index: choice.index,
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
                                        sent_tool_calls = true;
                                    }
                                }
                                Some(visible)
                            } else if is_thinking {
                                None
                            } else {
                                Some(content.clone())
                            };

                            let _ = tx.try_send(Ok(ChatCompletionChunk {
                                id: response_id.clone(),
                                object: "chat.completion.chunk".to_string(),
                                created: current_timestamp(),
                                model: model_id.clone(),
                                choices: vec![ChunkChoice {
                                    index: choice.index,
                                    delta: ChatMessageDelta {
                                        role,
                                        content: visible.filter(|v| !v.is_empty()),
                                        reasoning_content: if is_thinking { Some(content) } else { None },
                                        citations: None,
                                        tool_calls: None,
                                    },
                                    finish_reason: None,
                                }],
                                session_url: session_url.clone(),
                            }));
                        }
                    } else {
                        line_buf.push(ch);
                    }
                }
            }

            // Flush remaining partial line
            let remaining = line_buf.trim().to_string();
            if !remaining.is_empty() && remaining != "data: [DONE]" {
                // Try to parse it as a final SSE event
                if let Some(json_str) = remaining.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<QwenSSEEvent>(json_str) {
                        if let Some(created) = &event.response_created {
                            if let Some(parent_id) = &created.parent_id {
                                store.store_last_message_id(&gateway_session_id_owned, parent_id).await;
                            }
                        }
                        for choice in event.choices {
                            if let Some(content) = choice.delta.content {
                                if !content.is_empty() {
                                    let phase = choice.delta.phase.as_deref();
                                    let is_thinking = phase == Some("think") || phase == Some("thinking_summary");
                                    if !is_thinking {
                                        full_content_buf.push_str(&content);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Flush any pending MTP block at stream end.
            if has_tools {
                mtp_state.finish(&tool_defs);
                for tc in mtp_state.collected_tool_calls.drain(..) {
                    if !native_calls_list.iter().any(|c| c.id == tc.id) {
                        store.store_tool_calls(&gateway_session_id_owned, std::slice::from_ref(&tc)).await;
                        let _ = tx.try_send(Ok(ChatCompletionChunk {
                            id: response_id.clone(),
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
                        sent_tool_calls = true;
                    }
                }
            }

            let final_fr = if sent_tool_calls { "tool_calls" } else { "stop" };

            let _ = tx.try_send(Ok(ChatCompletionChunk {
                id: response_id.clone(),
                object: "chat.completion.chunk".to_string(),
                created: current_timestamp(),
                model: model_id.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChatMessageDelta::default(),
                    finish_reason: Some(final_fr.to_string()),
                }],
                session_url: session_url.clone(),
            }));

            // The upstream chat is intentionally kept alive: the Qwen web app
            // persists every chat in the sidebar, and the store reuses the same
            // chat_id for the next turn (session continuation) or tool
            // roundtrip. Deleting it here broke both with CHAT_NOT_FOUND.
        });

        Ok(rx_stream.boxed())
    }
}

/// Rough token estimation using character count heuristic.
fn estimate_cost(prompt: &str, completion: &str) -> Usage {
    let prompt_tokens = (prompt.len() / 4).max(1) as i32;
    let completion_tokens = (completion.len() / 4).max(1) as i32;
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    }
}
