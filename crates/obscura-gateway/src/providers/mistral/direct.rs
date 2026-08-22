use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatContent,
    ChatMessage, ChatMessageDelta, ChunkChoice, FunctionCall, ToolCall, Usage,
};
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::streaming_upload::download_and_hash_batch;
use crate::session::SessionHandle;

use super::state::{MistralSessionState, MistralSessionStore};
use super::upload::{self, MistralFile, UploadCache};

const BASE_URL: &str = "https://chat.mistral.ai";

/// Feature ids understood by the Le Chat backend (module `088.feyq_gh1p.js`).
const FEATURE_REASONING: &str = "beta-reasoning";
const FEATURE_DEEP_RESEARCH: &str = "beta-deep-research";
const FEATURE_WEB_SEARCH: &str = "beta-websearch";

/// `webSupportedTaskCallbacks` list, extracted verbatim from module `2819141`.
const WEB_SUPPORTED_TASK_CALLBACKS: &[&str] = &[
    "ask_user_question",
    "ask_user_confirmation",
    "enable_connector",
    "ask_retry_or_continue_rate_limit",
    "collect_workflow_input",
    "delegate_workflow_execution",
];

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Maximum array index accepted in a fast-json-patch path, preventing hostile
/// patches from forcing huge allocations.
const MAX_PATCH_ARRAY_INDEX: usize = 100_000;

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Session token embedded in `session_url` and used as the store key, so a
/// conversation continues regardless of which pool browser session served the
/// previous turn (mirrors the kimi provider's scheme).
fn new_session_token() -> String {
    format!("mistral_{}", uuid::Uuid::new_v4().simple())
}

fn make_session_url(token: &str, chat_id: &str) -> String {
    format!("mistral://session/{token}?chat_id={chat_id}")
}

fn extract_session_token(url: &str) -> Option<String> {
    url.strip_prefix("mistral://session/")
        .and_then(|s| s.split('?').next())
        .map(|s| s.to_string())
}

fn build_stealth_client(session: &SessionHandle) -> Arc<obscura_net::StealthHttpClient> {
    Arc::new(obscura_net::StealthHttpClient::new(session.cookie_jar.clone()))
}

/// Send a request through the stealth client and return a streaming response,
/// retrying transient 5xx/429 statuses. The wreq Chrome TLS emulation is what
/// clears the Cloudflare gate on chat.mistral.ai; a plain rustls reqwest
/// fingerprint is answered with a 403 challenge page (docs/MISTRAL_PROTOCOL.md).
async fn send_stealth_stream(
    stealth: &obscura_net::StealthHttpClient,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: &str,
) -> Result<obscura_net::StreamingResponse, GatewayError> {
    let parsed_url = url::Url::parse(url)
        .map_err(|e| GatewayError::Internal(format!("invalid URL {url}: {e}")))?;

    for attempt in 1..=3 {
        match stealth
            .send_single_streaming(method, &parsed_url, headers, body)
            .await
        {
            Ok(resp) => {
                let code = resp.status;
                if code >= 500 || code == 429 {
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64))
                            .await;
                        continue;
                    }
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64))
                        .await;
                    continue;
                }
                return Err(GatewayError::Internal(format!("stealth request failed: {e}")));
            }
        }
    }
    Err(GatewayError::Internal(
        "stealth request exhausted retries".to_string(),
    ))
}

pub struct DirectClient {
    session: SessionHandle,
    model_id: String,
    stealth: Arc<obscura_net::StealthHttpClient>,
    store: MistralSessionStore,
    upload_cache: UploadCache,
}

impl DirectClient {
    pub async fn new(
        session: SessionHandle,
        model_id: &str,
        store: MistralSessionStore,
    ) -> Result<Self, GatewayError> {
        upload::validate_session_cookie(&session)?;
        let stealth = build_stealth_client(&session);
        Ok(Self {
            session,
            model_id: model_id.to_string(),
            stealth,
            store,
            upload_cache: UploadCache::new(),
        })
    }

    /// Resolve the feature list from the request toggles.
    ///
    /// Mirrors the web app's `applyAnswerModeToChatFeatures`: the `think`
    /// answer mode maps to `beta-reasoning`, the `research` answer mode maps
    /// to `beta-deep-research`, and web search is a separate tool feature.
    /// The deep-research model also enables `beta-deep-research` on its own.
    fn features(&self, request: &ChatCompletionRequest) -> Vec<String> {
        let mut features = Vec::new();
        if request.thinking.unwrap_or(false) {
            features.push(FEATURE_REASONING.to_string());
        }
        // Deep research is exposed through its dedicated model id. When the
        // caller selects it, enable the `beta-deep-research` feature so the
        // backend runs the research planner, matching the web app's research
        // answer mode.
        if self.model_id == "mistral-deepresearch-2507" {
            features.push(FEATURE_DEEP_RESEARCH.to_string());
        }
        if request.search.unwrap_or(false) {
            features.push(FEATURE_WEB_SEARCH.to_string());
        }
        features
    }

    /// The `messageInput` payload for the web API: the user's text, or an
    /// editor-content style array when files are attached.
    fn build_message_input(&self, text: &str) -> Value {
        json!([{ "type": "text", "text": text }])
    }

    /// Current date + timezone, mirroring `clientPromptData` in the web app.
    fn client_prompt_data(&self) -> Value {
        let now = chrono::Utc::now();
        json!({
            "currentDate": now.format("%Y-%m-%d").to_string(),
            "userTimezone": "+00:00",
        })
    }

    /// Build the create/append request body exactly as the web app does.
    ///
    /// The web app first creates the chat server-side via the tRPC
    /// `message.newChat` mutation, which returns a real `chatId`, then sends
    /// the message to `/api/chat` with `mode: "start"` (first message) or
    /// `mode: "append"` (continuation) plus that chatId. Every mode branch of
    /// the live API requires an existing chatId.
    fn build_request_body(
        &self,
        mode: &str,
        chat_id: &str,
        message_id: &str,
        files: &[MistralFile],
        features: &[String],
        text: &str,
        anonymous_identifier: &str,
    ) -> Result<Value, GatewayError> {
        let message_files: Vec<Value> = files
            .iter()
            .map(|f| json!({ "type": f.file_type, "url": f.url, "name": f.name }))
            .collect();

        let mut body = serde_json::Map::new();
        body.insert("chatId".to_string(), json!(chat_id));
        body.insert("mode".to_string(), json!(mode));
        body.insert("model".to_string(), json!(self.model_id));
        body.insert("boostMode".to_string(), json!(false));
        body.insert(
            "messageInput".to_string(),
            self.build_message_input(text),
        );
        body.insert("messageFiles".to_string(), json!(message_files));
        body.insert("messageId".to_string(), json!(message_id));
        body.insert("features".to_string(), json!(features));
        body.insert("libraries".to_string(), json!([]));
        body.insert("integrations".to_string(), json!([]));
        body.insert("clientPromptData".to_string(), self.client_prompt_data());
        body.insert(
            "stableAnonymousIdentifier".to_string(),
            json!(anonymous_identifier),
        );
        body.insert(
            "supportedTaskCallbacks".to_string(),
            json!(WEB_SUPPORTED_TASK_CALLBACKS),
        );
        body.insert("disabledFeatures".to_string(), json!([]));

        Ok(Value::Object(body))
    }

    /// Process message attachments into Mistral files.
    ///
    /// The web API keeps conversation history server-side and attaches files
    /// per turn (`messageFiles`), so only the latest user message's
    /// attachments are uploaded, mirroring the web app's create/append flow.
    async fn process_attachments(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<Vec<MistralFile>, GatewayError> {
        let mut files = Vec::new();
        let latest_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user");
        
        if let Some(message) = latest_user {
            let image_urls = message.content.image_urls();
            let file_urls = message.content.file_urls();
            
            // Separate data URIs from remote URLs
            let mut data_uris = Vec::new();
            let mut remote_urls = Vec::new();
            
            for url in image_urls.iter().chain(file_urls.iter()) {
                if url.starts_with("data:") {
                    data_uris.push(url.clone());
                } else {
                    remote_urls.push(url.clone());
                }
            }

            // Concurrently download and hash remote URLs
            if !remote_urls.is_empty() {
                let http = reqwest::Client::new();
                match download_and_hash_batch(&http, remote_urls).await {
                    Ok(hashed_files) => {
                        // Upload concurrently too; the upload is the latency
                        // bottleneck. Order is preserved by collecting results
                        // by index and reassembling in original order.
                        use futures::stream::{self, StreamExt};
                        let mut results: Vec<(usize, Result<crate::providers::mistral::upload::MistralFile, String>)> =
                            Vec::with_capacity(hashed_files.len());
                        let mut buffered = stream::iter(hashed_files.into_iter().enumerate())
                            .map(|(idx, hashed)| {
                                let session = self.session.clone();
                                let upload_cache = self.upload_cache.clone();
                                async move {
                                    let result = upload::upload_hashed_file(
                                        &hashed.bytes,
                                        &hashed.name,
                                        &session,
                                        &upload_cache,
                                    )
                                    .await
                                    .map_err(|e| e.to_string());
                                    (idx, result)
                                }
                            })
                            .buffered(4);
                        while let Some((idx, result)) = buffered.next().await {
                            results.push((idx, result));
                        }
                        results.sort_by_key(|(idx, _)| *idx);
                        for (_, result) in results {
                            match result {
                                Ok(file) => files.push(file),
                                Err(e) => {
                                    tracing::warn!(error = %e, "Mistral concurrent file upload failed, skipping");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Mistral concurrent download failed");
                    }
                }
            }

            // Process data URIs sequentially
            for url in data_uris {
                match upload::resolve_url(&url, &self.session, &self.upload_cache).await {
                    Ok(file) => files.push(file),
                    Err(e) => {
                        tracing::warn!(url = %url, error = %e, "Mistral data URI upload failed, skipping");
                    }
                }
            }
        }
        Ok(files)
    }

    /// Create a chat server-side via the tRPC `message.newChat` mutation, the
    /// step the web app performs before its first `/api/chat` `mode:"start"`
    /// call. Returns the real `chatId` the mutation resolves to.
    async fn create_chat(
        &self,
        text: &str,
        features: &[String],
        files: &[MistralFile],
    ) -> Result<String, GatewayError> {
        let message_files: Vec<Value> = files
            .iter()
            .map(|f| json!({ "type": f.file_type, "url": f.url, "name": f.name }))
            .collect();
        let url = format!("{BASE_URL}/api/trpc/message.newChat?batch=1");
        let body = json!({
            "0": {
                "json": {
                    "content": text,
                    "files": message_files,
                    "features": features,
                    "integrations": [],
                    "libraries": [],
                    "projectId": null,
                }
            }
        });
        let body_str = serde_json::to_string(&body)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert("origin".to_string(), BASE_URL.to_string());
        headers.insert("referer".to_string(), format!("{BASE_URL}/chat"));

        let resp = send_stealth_stream(&self.stealth, "POST", &url, &headers, &body_str).await?;
        let status = resp.status;
        if !(200..300).contains(&status) {
            let mut err_body = String::new();
            let mut stream = resp.bytes;
            while err_body.len() < 1000 {
                match stream.next().await {
                    Some(Ok(b)) => err_body.push_str(&String::from_utf8_lossy(&b)),
                    _ => break,
                }
            }
            return Err(GatewayError::Provider(format!(
                "Mistral chat creation failed ({status}): {}",
                err_body.chars().take(1000).collect::<String>()
            )));
        }

        let mut buf = Vec::new();
        let mut stream = resp.bytes;
        while let Some(item) = stream.next().await {
            match item {
                Ok(b) => buf.extend_from_slice(&b),
                Err(e) => {
                    return Err(GatewayError::Provider(format!(
                        "Mistral chat creation read error: {e}"
                    )));
                }
            }
        }
        let v: Value = serde_json::from_slice(&buf).map_err(|e| {
            GatewayError::Provider(format!(
                "Mistral chat creation invalid JSON ({} bytes): {e}",
                buf.len()
            ))
        })?;
        // tRPC batch envelope: `{"0":{"result":{"data":...}}}` (object form) or
        // `[{"result":{"data":...}}]` (array form). The mutation resolves to a
        // message object whose `chatId` sits at `data.json.messages.chatId` (or
        // `data.json.chat.id`), so walk the whole value and grab the first
        // string chatId.
        let chat_id = find_chat_id(&v).filter(|s| !s.is_empty());
        chat_id.ok_or_else(|| {
            GatewayError::Provider(format!(
                "Mistral chat creation returned no chatId: {}",
                String::from_utf8_lossy(&buf)
                    .chars()
                    .take(500)
                    .collect::<String>()
            ))
        })
    }

    /// Extract the last user text from the request, mirroring the gateway's
    /// convention for multi-turn history (the web API stores history server
    /// side, so only the latest turn is sent on continuation).
    fn latest_user_text(&self, request: &ChatCompletionRequest) -> String {
        let raw = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_text())
            .unwrap_or_default();
        // Compile client tools into the MTP system prompt (universal
        // dialect); native `tools` are never forwarded upstream.
        match request.tools.as_ref() {
            Some(tools) if !tools.is_empty() => format!(
                "{}\n\nUser request:\n{}",
                crate::providers::mtp::build_mtp_system_prompt(
                    tools,
                    request.tool_choice.as_ref(),
                    false
                ),
                raw
            ),
            _ => raw,
        }
    }

    /// Non-streaming chat: consume the full stream and return one response.
    pub async fn chat(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let response_id = format!("chatcmpl-{}", &new_uuid()[..8]);
        let model_id = request.model.clone();
        let text = self.latest_user_text(&request);
        let files = self.process_attachments(&request).await?;
        let features = self.features(&request);

        let stream = self
            .chat_stream_inner(request, &files, &features, &text)
            .await?;

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // The finish chunk carries the session_url for this conversation, so
        // it is captured for both new chats and continuations.
        let mut session_url: Option<String> = None;

        let mut stream = stream;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if let Some(url) = &chunk.session_url {
                session_url = Some(url.clone());
            }
            for choice in &chunk.choices {
                if let Some(delta) = &choice.delta.content {
                    full_content.push_str(delta);
                }
                if let Some(delta) = &choice.delta.reasoning_content {
                    full_reasoning.push_str(delta);
                }
                if let Some(calls) = &choice.delta.tool_calls {
                    tool_calls.extend(calls.iter().cloned());
                }
            }
        }

        let prompt_tokens = estimate_tokens("mistral", &model_id, &text);
        let completion_tokens = estimate_tokens("mistral", &model_id, &full_content);
        let usage = Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        };

        let message = ChatMessage {
            role: "assistant".to_string(),
            content: ChatContent::String(full_content),
            name: None,
            reasoning_content: if full_reasoning.is_empty() {
                None
            } else {
                Some(full_reasoning)
            },
            citations: None,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        };

        Ok(ChatCompletionResponse {
            id: response_id,
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: model_id,
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message,
                finish_reason: "stop".to_string(),
            }],
            usage,
            session_url,
        })
    }

    /// Streaming chat completion. Forwards each NDJSON event as a chunk.
    pub async fn chat_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError>
    {
        let text = self.latest_user_text(&request);
        let files = self.process_attachments(&request).await?;
        let features = self.features(&request);
        self.chat_stream_inner(request, &files, &features, &text)
            .await
    }

    async fn chat_stream_inner(
        &self,
        request: ChatCompletionRequest,
        files: &[MistralFile],
        features: &[String],
        text: &str,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError>
    {
        let response_id = format!("chatcmpl-{}", &new_uuid()[..8]);
        let model_id = request.model.clone();

        // Resolve continuation state by token, keyed on the client-supplied
        // session_url so the thread survives pool session rotation.
        let token = match request.session_url.as_deref().and_then(extract_session_token) {
            Some(t) => t,
            None => new_session_token(),
        };
        let stored = self.store.get(&token).await;
        let mut chat_id = stored.as_ref().map(|s| s.chat_id.clone());
        let anonymous_identifier = stored
            .as_ref()
            .map(|s| s.anonymous_identifier.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(new_uuid);
        let message_id = new_uuid();

        // First turn: the live API has no create-and-start mode, so create the
        // chat server-side via the tRPC `message.newChat` mutation (which is
        // what the web app does before its first `/api/chat` "start" call) and
        // persist the returned chatId so the turn survives stream failures.
        let is_new_chat = chat_id.is_none();
        if is_new_chat {
            let created = self.create_chat(text, features, files).await?;
            self.store
                .update(
                    &token,
                    MistralSessionState {
                        chat_id: created.clone(),
                        message_id: String::new(),
                        message_version: 0,
                        model: model_id.clone(),
                        features: features.to_vec(),
                        anonymous_identifier: anonymous_identifier.clone(),
                    },
                )
                .await;
            chat_id = Some(created);
        }

        let body = self.build_request_body(
            if is_new_chat { "start" } else { "append" },
            chat_id.as_deref().expect("chat_id set above"),
            &message_id,
            files,
            features,
            text,
            &anonymous_identifier,
        )?;

        let url = format!("{BASE_URL}/api/chat");
        let body_str = serde_json::to_string(&body)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert("origin".to_string(), BASE_URL.to_string());
        headers.insert(
            "referer".to_string(),
            chat_id
                .as_ref()
                .map(|c| format!("{BASE_URL}/chat/{c}"))
                .unwrap_or_else(|| format!("{BASE_URL}/chat")),
        );

        // The whole connection (headers + body stream) goes through the stealth
        // client so Cloudflare sees a real Chrome TLS/HTTP fingerprint. Cookies
        // are read from the shared jar per request by the stealth client.
        let resp = send_stealth_stream(&self.stealth, "POST", &url, &headers, &body_str).await?;
        let status = resp.status;
        if !(200..300).contains(&status) {
            // Drain a bounded prefix of the body for the error message;
            // challenge pages can be large HTML.
            let mut err_body = String::new();
            let mut stream = resp.bytes;
            while err_body.len() < 2000 {
                match stream.next().await {
                    Some(Ok(b)) => err_body.push_str(&String::from_utf8_lossy(&b)),
                    _ => break,
                }
            }
            return Err(GatewayError::Provider(format!(
                "Mistral chat failed ({status}): {}",
                err_body.chars().take(2000).collect::<String>()
            )));
        }

        let byte_stream = resp
            .bytes
            .map(|item| {
                item.map_err(|e| GatewayError::Provider(format!("Mistral stream read error: {e}")))
            })
            .boxed();

        let (tx, rx) = mpsc::unbounded_channel();
        let rx_stream = UnboundedReceiverStream::new(rx);

        let store = self.store.clone();
        let token_owned = token;
        let chat_id_hint = chat_id.clone();
        let features_owned = features.to_vec();
        let anonymous_identifier_owned = anonymous_identifier.clone();
        let tool_defs_owned = request.tools.clone().unwrap_or_default();

        tokio::spawn(async move {
            let result = consume_stream(
                byte_stream,
                &tx,
                &response_id,
                &model_id,
                &token_owned,
                &store,
                chat_id_hint.as_deref(),
                &features_owned,
                &anonymous_identifier_owned,
                &tool_defs_owned,
            )
            .await;
            if let Err(e) = result {
                let _ = tx.send(Err(e));
            }
        });

        Ok(rx_stream.boxed())
    }
}

/// Consume the NDJSON response stream, applying fast-json-patch `state_update`
/// events and emitting OpenAI-compatible chunks.
async fn consume_stream(
    mut byte_stream: BoxStream<'static, Result<Vec<u8>, GatewayError>>,
    tx: &mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
    response_id: &str,
    model_id: &str,
    token: &str,
    store: &MistralSessionStore,
    chat_id_hint: Option<&str>,
    features: &[String],
    anonymous_identifier: &str,
    tool_defs: &[crate::models::Tool],
) -> Result<(), GatewayError> {
    // MTP state absorbs [MIRAGE_TOOL_CALL_V1] blocks from content deltas.
    let mut mtp_state = crate::providers::mtp::MtpStreamState::new();
    // Byte buffer: lines are split on `\n` and only complete lines are decoded
    // to UTF-8, so a multi-byte character split across network chunks is never
    // corrupted (avoids `from_utf8_lossy` per chunk).
    let mut line_buf: Vec<u8> = Vec::new();
    const MAX_LINE_BUF: usize = 1024 * 1024;
    let mut chat_id: Option<String> = chat_id_hint.map(|s| s.to_string());
    let mut message_id: Option<String> = None;
    let mut message_version: i64 = 0;
    let mut sent_role = false;
    // Accumulated patch-applied message JSON keyed by (messageId). The stream
    // interleaves patches for several messages (assistant content, a
    // moderation message, ...), so each message accumulates independently;
    // resuming the assistant message after an unrelated one must not wipe it.
    let mut messages: HashMap<String, Value> = HashMap::new();
    let mut last_content = String::new();
    let mut last_reasoning = String::new();
    // Accumulated tool calls (deduplicated by id, snapshot form).
    let mut seen_tool_calls: Vec<ToolCall> = Vec::new();

    let mut finished = false;

    while let Some(chunk) = byte_stream.next().await {
        let chunk = match chunk {
            Ok(b) => b,
            Err(e) => {
                return Err(e);
            }
        };

        line_buf.extend_from_slice(&chunk);
        if line_buf.len() > MAX_LINE_BUF {
            return Err(GatewayError::Provider(
                "Mistral stream line exceeded the 1 MB buffer limit".to_string(),
            ));
        }

        while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
            let line_bytes = line_buf[..pos].to_vec();
            line_buf.drain(..=pos);
            let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
            if line.is_empty() {
                continue;
            }

            // Parse `<code>:<json>` (optionally `__context__:<json>` suffix).
            let Some((code, value)) = parse_stream_line(&line) else {
                tracing::debug!(line = %line.chars().take(200).collect::<String>(), "unparseable Mistral stream line");
                continue;
            };
            // Event payloads for state/error codes are wrapped as
            // `{"json":{...}}`; unwrap so `type`/`patches` are directly visible.
            let value = value.get("json").cloned().unwrap_or(value);

            match code {
                0 => {
                    // Text delta part (value is a string).
                    if let Some(text) = value.as_str() {
                        if !text.is_empty() {
                            if !sent_role {
                                send_delta(
                                    tx,
                                    response_id,
                                    model_id,
                                    Some("assistant".to_string()),
                                    None,
                                    None,
                                    None,
                                    None,
                                );
                                sent_role = true;
                            }
                            let delta = text.to_string();
                            // Track against the shared accumulator so later
                            // code-15 patches emitting the full content only
                            // forward the remaining delta.
                            last_content.push_str(&delta);
                            let visible = feed_mtp(&mut mtp_state, tool_defs, &delta);
                            let _ = tx.send(Ok(build_chunk(
                                response_id,
                                model_id,
                                visible,
                                None,
                                None,
                                None,
                            )));
                        }
                    }
                }
                8 => {
                    // done
                    finished = true;
                }
                6 => {
                    // error
                    let msg = value
                        .get("message")
                        .or_else(|| value.get("detail"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown Mistral stream error");
                    return Err(GatewayError::Provider(format!("Mistral error: {msg}")));
                }
                15 => {
                    // state_update
                    match value.get("type").and_then(|v| v.as_str()) {
                        Some("bootstrap") => {
                            // {chat, messages, canvas}
                            if let Some(chat) = value.get("chat") {
                                if let Some(cid) =
                                    chat.get("id").and_then(|v| v.as_str())
                                {
                                    chat_id = Some(cid.to_string());
                                }
                            }
                            // Persist the session so continuation can reuse it.
                            if let Some(cid) = chat_id.clone() {
                                store
                                    .update(
                                        token,
                                        MistralSessionState {
                                            chat_id: cid,
                                            message_id: message_id
                                                .clone()
                                                .unwrap_or_default(),
                                            message_version,
                                            model: model_id.to_string(),
                                            features: features.to_vec(),
                                            anonymous_identifier: anonymous_identifier
                                                .to_string(),
                                        },
                                    )
                                    .await;
                            }
                        }
                        Some("message") => {
                            let mid = value
                                .get("messageId")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let ver = value
                                .get("messageVersion")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let Some(mid) = mid else {
                                break;
                            };
                            message_id = Some(mid.clone());
                            message_version = ver;

                            // Cap the accumulator so a hostile stream cannot
                            // grow it unbounded (robustness invariant).
                            if messages.len() >= 64 {
                                messages.clear();
                            }
                            let patches = value
                                .get("patches")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();

                            // Apply patches to this message's accumulated JSON.
                            let msg = messages
                                .entry(mid.clone())
                                .or_insert_with(|| json!({}));
                            if let Err(e) = apply_patches(msg, &patches) {
                                tracing::debug!(error = %e, "Mistral patch apply failed");
                            }

                            // Only content-bearing messages (those carrying
                            // contentChunks or a content string) advance the
                            // delta trackers; the moderation/other side
                            // messages must not disturb the assistant stream.
                            let has_content = msg.get("contentChunks").is_some()
                                || msg
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .is_some();
                            if has_content {
                                // Extract content (skip reasoning chunks).
                                let content = extract_text_content(msg);
                                if content != last_content {
                                    let delta = if content.starts_with(&last_content) {
                                        content[last_content.len()..].to_string()
                                    } else {
                                        content.clone()
                                    };
                                    if !delta.is_empty() {
                                        if !sent_role {
                                            send_delta(
                                                tx,
                                                response_id,
                                                model_id,
                                                Some("assistant".to_string()),
                                                None,
                                                None,
                                                None,
                                                None,
                                            );
                                            sent_role = true;
                                        }
                                        let visible = feed_mtp(&mut mtp_state, tool_defs, &delta);
                                        let _ = tx.send(Ok(build_chunk(
                                            response_id,
                                            model_id,
                                            visible,
                                            None,
                                            None,
                                            None,
                                        )));
                                    }
                                    last_content = content;
                                }

                                // Extract reasoning.
                                let reasoning = extract_reasoning_content(msg);
                                if reasoning != last_reasoning {
                                    let delta = if reasoning.starts_with(&last_reasoning) {
                                        reasoning[last_reasoning.len()..].to_string()
                                    } else {
                                        reasoning.clone()
                                    };
                                    if !delta.is_empty() {
                                        let _ = tx.send(Ok(build_chunk(
                                            response_id,
                                            model_id,
                                            None,
                                            Some(delta),
                                            None,
                                            None,
                                        )));
                                    }
                                    last_reasoning = reasoning;
                                }

                                // Extract tool calls.
                                if let Some(calls) = extract_tool_calls(msg) {
                                    for call in calls {
                                        if !seen_tool_calls
                                            .iter()
                                            .any(|s| s.id == call.id)
                                        {
                                            seen_tool_calls.push(call.clone());
                                            let _ = tx.send(Ok(build_chunk(
                                                response_id,
                                                model_id,
                                                None,
                                                None,
                                                Some(vec![call]),
                                                None,
                                            )));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {
                    // 1 moderation, 2 references, 3 canva, 4 canva_token,
                    // 5 tool_call (full object), 7 image, 9 references_ids,
                    // 10 file_reference, 11 widget, 12 reasoning,
                    // 13 deep_research_event, 14 tool_reference, 16 disclaimer
                    if code == 12 {
                        // Direct reasoning part.
                        if let Some(text) = value.as_str() {
                            if !text.is_empty() {
                                let _ = tx.send(Ok(build_chunk(
                                    response_id,
                                    model_id,
                                    None,
                                    Some(text.to_string()),
                                    None,
                                    None,
                                )));
                                // Track against the shared accumulator so
                                // later code-15 patches emitting the full
                                // reasoning only forward the remaining delta.
                                last_reasoning.push_str(text);
                            }
                        }
                    } else if code == 5 {
                        // Direct tool_call part.
                        if let Some(call) = tool_call_from_value(&value) {
                            if !seen_tool_calls.iter().any(|s| s.id == call.id) {
                                seen_tool_calls.push(call.clone());
                                let _ = tx.send(Ok(build_chunk(
                                    response_id,
                                    model_id,
                                    None,
                                    None,
                                    Some(vec![call]),
                                    None,
                                )));
                            }
                        }
                    }
                }
            }

            if finished {
                break;
            }
        }

        if finished {
            break;
        }
    }

    // Flush any pending MTP block and emit collected calls.
    if !tool_defs.is_empty() {
        mtp_state.finish(tool_defs);
        if !mtp_state.collected_tool_calls.is_empty() {
            let _ = tx.send(Ok(build_chunk(
                response_id,
                model_id,
                None,
                None,
                Some(std::mem::take(&mut mtp_state.collected_tool_calls)),
                Some("tool_calls".to_string()),
            )));
        }
    }

    // Finalize: persist message id/version and emit the finish chunk. A
    // truncated stream (EOF without a code-8 `done`) must not look complete:
    // report an error instead.
    if !finished {
        return Err(GatewayError::Provider(
            "Mistral stream ended before receiving the done event".to_string(),
        ));
    }

    let session_url = chat_id
        .as_ref()
        .map(|c| make_session_url(token, c));
    if let Some(cid) = chat_id {
        store
            .update(
                token,
                MistralSessionState {
                    chat_id: cid,
                    message_id: message_id.unwrap_or_default(),
                    message_version,
                    model: model_id.to_string(),
                    features: features.to_vec(),
                    anonymous_identifier: anonymous_identifier.to_string(),
                },
            )
            .await;
    }

    let _ = tx.send(Ok(ChatCompletionChunk {
        id: response_id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model_id.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta::default(),
            finish_reason: Some("stop".to_string()),
        }],
        session_url,
    }));

    Ok(())
}

/// Parse a Mistral NDJSON stream line: `{code}:{json}` with an optional
/// `__context__:` suffix. The code-8 `done` event may carry an empty payload
/// (`8:`), which is accepted and represented as `null`.
fn parse_stream_line(line: &str) -> Option<(i64, Value)> {
    let idx = line.find(':')?;
    let code: i64 = line[..idx].parse().ok()?;
    let rest = &line[idx + 1..];
    // Split off `__context__:` if present.
    let value_str = match rest.split_once("__context__:") {
        Some((v, _ctx)) => v,
        None => rest,
    };
    let value_str = value_str.trim();
    if code == 8 && value_str.is_empty() {
        return Some((code, Value::Null));
    }
    // For codes >= 15 the value is parsed with the fast-json-patch parser,
    // which for the message/canvas/chat patch payloads is plain JSON.
    let value: Value = serde_json::from_str(value_str).ok()?;
    Some((code, value))
}

/// Apply a subset of fast-json-patch ops to `doc`.
///
/// Supported ops (from the web app's `createPatchSchema` in module `5795308`):
/// - `replace` path `/`  : replace the whole document (root patch)
/// - `replace` / `add`   : set a value at a JSON-pointer path
/// - `remove`            : delete a value at a path
/// - `append`            : append a string to the string at a path
///
/// Array index `-` means "end of array" (add only).
fn apply_patches(doc: &mut Value, patches: &[Value]) -> Result<(), String> {
    for patch in patches {
        let op = patch
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "patch missing op".to_string())?;
        let path = patch
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("/")
            .to_string();

        match op {
            "replace" if path == "/" || path.is_empty() => {
                if let Some(value) = patch.get("value") {
                    *doc = value.clone();
                }
            }
            "append" => {
                let value = patch
                    .get("value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "append patch value must be a string".to_string())?;
                let parent = get_mut_at_path(doc, &path)?;
                let cur = parent.as_str().unwrap_or("");
                *parent = json!(format!("{cur}{value}"));
            }
            "replace" | "add" => {
                let value = patch.get("value").cloned().unwrap_or(Value::Null);
                set_at_path(doc, &path, value)?;
            }
            "remove" => {
                remove_at_path(doc, &path)?;
            }
            other => {
                tracing::debug!(op = other, path = %path, "unhandled Mistral patch op");
            }
        }
    }
    Ok(())
}

/// Resolve a JSON-pointer path (e.g. `/contentChunks/0/text`) to a mutable
/// reference of the final segment's parent container. Returns the mutable
/// value slot for the last segment. Numeric segments index into arrays, so an
/// append to a string inside an array element (`/contentChunks/0/text`) works.
fn get_mut_at_path<'a>(doc: &'a mut Value, path: &str) -> Result<&'a mut Value, String> {
    let tokens = path.split('/').filter(|t| !t.is_empty()).collect::<Vec<_>>();
    if tokens.is_empty() {
        return Ok(doc);
    }
    let mut cur = doc;
    for (i, token) in tokens.iter().enumerate() {
        let is_last = i == tokens.len() - 1;
        let slot = if let Ok(idx) = token.parse::<usize>() {
            let arr = cur
                .as_array_mut()
                .ok_or_else(|| format!("segment '{token}' requires an array"))?;
            arr.get_mut(idx)
                .ok_or_else(|| format!("array index {idx} out of range in '{path}'"))?
        } else {
            cur.get_mut(*token)
                .ok_or_else(|| format!("path segment '{token}' not found in '{path}'"))?
        };
        if is_last {
            return Ok(slot);
        }
        cur = slot;
    }
    Ok(cur)
}

/// Set a value at a JSON-pointer path, creating intermediate objects/arrays.
///
/// Recursive implementation with no raw pointers: each level reborrows the
/// tree from the parent, and the terminal segment inserts the value.
fn set_at_path(doc: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let tokens = path.split('/').filter(|t| !t.is_empty()).collect::<Vec<_>>();
    if tokens.is_empty() {
        *doc = value;
        return Ok(());
    }
    set_at_tokens(doc, &tokens, value)
}

fn set_at_tokens(doc: &mut Value, tokens: &[&str], value: Value) -> Result<(), String> {
    let (token, rest) = tokens
        .split_first()
        .ok_or_else(|| "empty path".to_string())?;

    if rest.is_empty() {
        if *token == "-" {
            let arr = doc
                .as_array_mut()
                .ok_or_else(|| "append index '-' requires an array".to_string())?;
            arr.push(value);
            return Ok(());
        }
        if let Ok(idx) = token.parse::<usize>() {
            let arr = doc
                .as_array_mut()
                .ok_or_else(|| format!("index '{token}' requires an array"))?;
            if idx < arr.len() {
                arr[idx] = value;
            } else if idx == arr.len() {
                arr.push(value);
            } else {
                return Err(format!("array index {idx} out of range"));
            }
            return Ok(());
        }
        let obj = doc
            .as_object_mut()
            .ok_or_else(|| format!("segment '{token}' requires an object"))?;
        obj.insert(token.to_string(), value);
        return Ok(());
    }

    // Descend into the child, creating it when missing. Cap the array size so
    // a hostile patch path like `/contentChunks/999999999` cannot force a
    // huge allocation (robustness invariant: one stream never OOMs a worker).
    if let Ok(idx) = token.parse::<usize>() {
        if !doc.is_array() {
            *doc = Value::Array(Vec::new());
        }
        let arr = doc
            .as_array_mut()
            .ok_or_else(|| "expected array".to_string())?;
        if idx >= arr.len() {
            if idx > MAX_PATCH_ARRAY_INDEX {
                return Err(format!("array index {idx} exceeds safety cap"));
            }
            arr.resize(idx + 1, Value::Null);
        }
        set_at_tokens(&mut arr[idx], rest, value)
    } else {
        if !doc.is_object() {
            *doc = Value::Object(serde_json::Map::new());
        }
        let obj = doc
            .as_object_mut()
            .ok_or_else(|| "expected object".to_string())?;
        if !obj.contains_key(*token) {
            obj.insert(token.to_string(), Value::Object(serde_json::Map::new()));
        }
        set_at_tokens(
            obj.get_mut(*token)
                .ok_or_else(|| "path not found".to_string())?,
            rest,
            value,
        )
    }
}

/// Remove a value at a JSON-pointer path.
fn remove_at_path(doc: &mut Value, path: &str) -> Result<(), String> {
    let tokens = path.split('/').filter(|t| !t.is_empty()).collect::<Vec<_>>();
    if tokens.is_empty() {
        *doc = Value::Null;
        return Ok(());
    }
    let mut cur = doc;
    for (i, token) in tokens.iter().enumerate() {
        let is_last = i == tokens.len() - 1;
        if is_last {
            if let Ok(idx) = token.parse::<usize>() {
                let arr = cur
                    .as_array_mut()
                    .ok_or_else(|| format!("index '{token}' requires an array"))?;
                if idx < arr.len() {
                    arr.remove(idx);
                }
            } else if cur.is_object() {
                cur.as_object_mut()
                    .ok_or_else(|| "expected object".to_string())?
                    .remove(*token);
            }
            return Ok(());
        }
        cur = cur
            .get_mut(*token)
            .ok_or_else(|| format!("path segment '{token}' not found in '{path}'"))?;
    }
    Ok(())
}

/// Extract the plain-text content of a patched Mistral message, skipping
/// reasoning chunks (they are tagged `_context.type == "reasoning"`).
fn extract_text_content(msg: &Value) -> String {
    // The bootstrap patch seeds `content: ""`, so only trust a non-empty
    // string here; the real text streams into `contentChunks` below.
    if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            return content.to_string();
        }
    }
    // Fall back to concatenating text chunks from contentChunks.
    if let Some(chunks) = msg.get("contentChunks").and_then(|v| v.as_array()) {
        let mut out = String::new();
        for chunk in chunks {
            let is_reasoning = chunk
                .get("_context")
                .and_then(|c| c.get("type"))
                .and_then(|v| v.as_str())
                == Some("reasoning");
            if is_reasoning {
                continue;
            }
            if chunk.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(t) = chunk.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                }
            }
        }
        return out;
    }
    String::new()
}

/// Extract the reasoning text of a patched Mistral message.
fn extract_reasoning_content(msg: &Value) -> String {
    if let Some(chunks) = msg.get("contentChunks").and_then(|v| v.as_array()) {
        let mut out = String::new();
        for chunk in chunks {
            let is_reasoning = chunk
                .get("_context")
                .and_then(|c| c.get("type"))
                .and_then(|v| v.as_str())
                == Some("reasoning");
            if !is_reasoning {
                continue;
            }
            if let Some(t) = chunk.get("text").and_then(|v| v.as_str()) {
                out.push_str(t);
            }
        }
        return out;
    }
    // Some messages carry reasoning in a dedicated field.
    if let Some(r) = msg.get("reasoning").and_then(|v| v.as_str()) {
        return r.to_string();
    }
    String::new()
}

/// Extract tool calls from a patched Mistral message's `contentChunks`.
fn extract_tool_calls(msg: &Value) -> Option<Vec<ToolCall>> {
    let chunks = msg.get("contentChunks").and_then(|v| v.as_array())?;
    let calls: Vec<ToolCall> = chunks
        .iter()
        .filter(|c| c.get("type").and_then(|v| v.as_str()) == Some("tool_call"))
        .filter_map(tool_call_from_value)
        .collect();
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// Recursively find the first non-empty `chatId` string in a value. Used to
/// parse the `message.newChat` mutation's tRPC batch envelope, where the id
/// lives at `data.json.messages.chatId` (or `data.json.chat.id`).
fn find_chat_id(v: &Value) -> Option<String> {
    match v {
        Value::String(_) => None,
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("chatId") {
                return Some(id.clone());
            }
            if let Some(Value::Object(chat)) = map.get("chat") {
                if let Some(Value::String(id)) = chat.get("id") {
                    return Some(id.clone());
                }
            }
            map.values().find_map(find_chat_id)
        }
        Value::Array(arr) => arr.iter().find_map(find_chat_id),
        _ => None,
    }
}

/// Convert a Mistral `tool_call` chunk/value into an OpenAI `ToolCall`.
fn tool_call_from_value(chunk: &Value) -> Option<ToolCall> {
    let name = chunk.get("name").and_then(|v| v.as_str())?.to_string();
    let id = chunk
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));

    let arguments = chunk
        .get("publicArguments")
        .cloned()
        .unwrap_or(Value::Null);
    let arguments_str = if let Some(s) = arguments.as_str() {
        s.to_string()
    } else if arguments.is_null() {
        "{}".to_string()
    } else {
        serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string())
    };

    Some(ToolCall {
        id,
        r#type: "function".to_string(),
        function: FunctionCall {
            name,
            arguments: arguments_str,
        },
    })
}

/// Feed one content delta through the MTP stream state. Returns the
/// user-visible text (tool blocks absorbed), or None when nothing remains.
fn feed_mtp(
    state: &mut crate::providers::mtp::MtpStreamState,
    tool_defs: &[crate::models::Tool],
    delta: &str,
) -> Option<String> {
    if tool_defs.is_empty() {
        return Some(delta.to_string());
    }
    let visible = state.process_delta(delta, tool_defs);
    if visible.is_empty() && state.collected_tool_calls.is_empty() {
        None
    } else {
        Some(visible)
    }
}

fn build_chunk(
    response_id: &str,
    model_id: &str,
    content: Option<String>,
    reasoning: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
    session_url: Option<String>,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: response_id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model_id.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta {
                role: None,
                content,
                reasoning_content: reasoning,
                citations: None,
                tool_calls,
            },
            finish_reason: None,
        }],
        session_url,
    }
}

fn send_delta(
    tx: &mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
    response_id: &str,
    model_id: &str,
    role: Option<String>,
    content: Option<String>,
    reasoning: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
    session_url: Option<String>,
) {
    let _ = tx.send(Ok(ChatCompletionChunk {
        id: response_id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model_id.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta {
                role,
                content,
                reasoning_content: reasoning,
                citations: None,
                tool_calls,
            },
            finish_reason: None,
        }],
        session_url,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_token_round_trips_through_url() {
        let token = new_session_token();
        assert!(token.starts_with("mistral_"));
        let url = make_session_url(&token, "chat-abc");
        assert_eq!(extract_session_token(&url).as_deref(), Some(token.as_str()));
        assert_eq!(
            extract_session_token("https://chat.mistral.ai/chat/abc"),
            None
        );
        assert_eq!(extract_session_token("not-a-session-url"), None);
    }

    #[test]
    fn apply_patches_root_replace() {
        let mut doc = json!({"old": true});
        apply_patches(
            &mut doc,
            &[json!({"op": "replace", "path": "/", "value": {"content": "hi"}})],
        )
        .unwrap();
        assert_eq!(doc, json!({"content": "hi"}));
    }

    #[test]
    fn apply_patches_append_string() {
        let mut doc = json!({"content": "he"});
        apply_patches(
            &mut doc,
            &[json!({"op": "append", "path": "/content", "value": "llo"})],
        )
        .unwrap();
        assert_eq!(doc.get("content").unwrap(), "hello");
    }

    #[test]
    fn apply_patches_add_and_remove() {
        let mut doc = json!({"contentChunks": []});
        apply_patches(
            &mut doc,
            &[
                json!({"op": "add", "path": "/contentChunks/-", "value": {"type": "text", "text": "a"}}),
                json!({"op": "remove", "path": "/contentChunks/0"}),
            ],
        )
        .unwrap();
        assert_eq!(doc.get("contentChunks").unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn apply_patches_rejects_hostile_array_index() {
        let mut doc = json!({"contentChunks": []});
        let err = apply_patches(
            &mut doc,
            &[json!({"op": "add", "path": "/contentChunks/999999999", "value": {}})],
        )
        .unwrap_err();
        assert!(
            err.contains("safety cap") || err.contains("out of range"),
            "err: {err}"
        );
    }

    #[test]
    fn extract_text_skips_reasoning_chunks() {
        let msg = json!({
            "contentChunks": [
                {"type": "text", "text": "Hello "},
                {"type": "text", "text": "world", "_context": {"type": "reasoning"}},
                {"type": "text", "text": "!"},
            ]
        });
        assert_eq!(extract_text_content(&msg), "Hello !");
        assert_eq!(extract_reasoning_content(&msg), "world");
    }

    #[test]
    fn tool_call_from_value_builds_openai_call() {
        let v = json!({
            "id": "call_1",
            "name": "web_search",
            "publicArguments": {"query": "rust"},
            "isDone": true,
        });
        let call = tool_call_from_value(&v).unwrap();
        assert_eq!(call.id, "call_1");
        assert_eq!(call.r#type, "function");
        assert_eq!(call.function.name, "web_search");
        assert_eq!(call.function.arguments, "{\"query\":\"rust\"}");
    }

    #[test]
    fn tool_call_from_value_defaults_id_and_args() {
        let v = json!({"name": "web_search", "isDone": true});
        let call = tool_call_from_value(&v).unwrap();
        assert!(call.id.starts_with("call_"));
        assert_eq!(call.function.arguments, "{}");
    }

    #[test]
    fn apply_patches_appends_string_inside_array_element() {
        // The reasoning/final-answer stream splices tokens into
        // `/contentChunks/<i>/text`; this must traverse the array index.
        let mut doc = json!({"contentChunks": [{"type": "text", "text": "P"}]});
        apply_patches(
            &mut doc,
            &[json!({"op": "append", "path": "/contentChunks/0/text", "value": "ONG"})],
        )
        .unwrap();
        assert_eq!(doc["contentChunks"][0]["text"], "PONG");
    }

    #[test]
    fn extract_text_falls_back_from_empty_content_to_chunks() {
        // The bootstrap patch seeds `content: ""` while the real text streams
        // into `contentChunks`; extraction must not stop on the empty string.
        let msg = json!({
            "content": "",
            "contentChunks": [{"type": "text", "text": "P", "_context": null}]
        });
        assert_eq!(extract_text_content(&msg), "P");
    }

    #[test]
    fn find_chat_id_walks_nested_trpc_data() {
        // The `message.newChat` mutation returns the chatId nested at
        // `data.json.messages.chatId` (and `data.json.chat.id`).
        let v = json!({
            "0": {"result": {"data": {"json": {
                "messages": {"chatId": "nested-abc", "role": "user"}
            }}}}
        });
        assert_eq!(find_chat_id(&v).as_deref(), Some("nested-abc"));

        let array_form = json!([{"result": {"data": {"json": {
            "messages": {"chatId": "arr-abc"}
        }}}}]);
        assert_eq!(find_chat_id(&array_form).as_deref(), Some("arr-abc"));

        let via_chat = json!({"data": {"json": {"chat": {"id": "chat-xyz"}}}});
        assert_eq!(find_chat_id(&via_chat).as_deref(), Some("chat-xyz"));

        assert_eq!(find_chat_id(&json!({"nope": 1})), None);
    }
}
