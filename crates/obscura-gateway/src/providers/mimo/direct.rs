use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::{BoxStream, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatContent,
    ChatMessage, ChatMessageDelta, ChunkChoice, Usage,
};
use crate::providers::mtp;
use crate::providers::streaming_upload::download_and_hash_batch;
use crate::session::SessionHandle;

use super::state::{MiMoSessionState, MiMoSessionStore};
use super::upload::{self, MediaItem, UploadCache};

const API_HOST: &str = "https://aistudio.xiaomimimo.com";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.12) Gecko/20100101 Firefox/140.12";

/// Map a public model id to the wire model id, matching the `model` field of
/// each entry in `GET /open-apis/bot/config` (the `name` field there is the
/// display name, e.g. the flash entry is `name: "mimo-v2-flash-studio"`,
/// `model: "mimo-v2-flash"`). Unknown ids fall back to the default model.
fn wire_model(public_model: &str) -> &'static str {
    match public_model {
        "mimo-v2.5-pro" => "mimo-v2.5-pro",
        "mimo-v2.5" => "mimo-v2.5",
        "mimo-v2-flash" | "mimo-v2-flash-studio" => "mimo-v2-flash",
        "mimo-v2-pro" => "mimo-v2-pro",
        "mimo-v2-omni" => "mimo-v2-omni",
        _ => "mimo-v2.5-pro",
    }
}

/// Whether thinking is enabled by default for a model. Matches
/// `thinkingDefaultOn` in `GET /open-apis/bot/config`: all models currently
/// default thinking on.
fn default_thinking(_public_model: &str) -> bool {
    true
}


fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Build a full cookie header for aistudio.xiaomimimo.com from the session
/// cookie jar. Returns None when the auth cookies are missing.
fn build_cookie_header(session: &SessionHandle) -> Result<String, GatewayError> {
    let all = session.cookie_jar.get_all_cookies();
    let filtered: Vec<obscura_net::CookieInfo> = all
        .into_iter()
        .filter(|c| {
            c.domain.contains("xiaomimimo.com")
                || c.domain.contains("xiaomichatbot")
                || c.domain.contains("mimo")
        })
        .collect();
    upload::validate_mimo_cookies(&filtered)?;
    Ok(upload::build_mimo_cookie_header(&filtered))
}

fn build_reqwest_client() -> Result<reqwest::Client, GatewayError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(UA));
    headers.insert(
        "accept",
        HeaderValue::from_static("text/event-stream, application/json"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| GatewayError::Internal(format!("failed to build reqwest client: {e}")))
}

pub struct DirectClient {
    model_id: String,
    cookie_header: String,
    ph: String,
    store: MiMoSessionStore,
    upload_cache: UploadCache,
}

impl DirectClient {
    pub async fn new(
        session: SessionHandle,
        model_id: &str,
        store: MiMoSessionStore,
    ) -> Result<Self, GatewayError> {
        let cookie_header = build_cookie_header(&session)?;
        let all = session.cookie_jar.get_all_cookies();
        let filtered: Vec<obscura_net::CookieInfo> = all
            .into_iter()
            .filter(|c| {
                c.domain.contains("xiaomimimo.com")
                    || c.domain.contains("xiaomichatbot")
                    || c.domain.contains("mimo")
            })
            .collect();
        let ph = upload::extract_ph(&filtered)
            .ok_or_else(|| {
                GatewayError::Auth(
                    "MiMo 'xiaomichatbot_ph' cookie not found. \
                     Log in to https://aistudio.xiaomimimo.com in your browser and re-run."
                        .to_string(),
                )
            })?
            .to_string();

        Ok(Self {
            model_id: model_id.to_string(),
            cookie_header,
            ph,
            store,
            upload_cache: UploadCache::new(),
        })
    }

    /// Resolve the conversation id for this gateway session.
    ///
    /// Reuses the persisted `conversationId` when one exists (session
    /// continuation), otherwise starts a fresh thread. The caller passes the
    /// optional `session_url` from a previous completion so a client can carry
    /// the same thread across different gateway sessions.
    async fn resolve_conversation(
        &self,
        gateway_session_id: &str,
        session_url: &Option<String>,
    ) -> MiMoSessionState {
        // 1. Prefer an explicit continuation URL from the client.
        if let Some(url) = session_url {
            if let Some(conv) = extract_conversation_id(url) {
                if let Some(existing) = self.store.get(gateway_session_id).await {
                    if existing.conversation_id != conv {
                        let mut state = existing.clone();
                        state.conversation_id = conv;
                        self.store.update(gateway_session_id, state).await;
                    }
                } else {
                    self.store
                        .insert(
                            gateway_session_id.to_string(),
                            MiMoSessionState {
                                conversation_id: conv,
                                model: self.model_id.clone(),
                                enable_thinking: default_thinking(&self.model_id),
                                web_search_status: "disabled".to_string(),
                            },
                        )
                        .await;
                }
                if let Some(state) = self.store.get(gateway_session_id).await {
                    return state;
                }
            }
        }

        // 2. Without an explicit continuation URL, always start a fresh thread.
        //    The gateway session id is a pooled browser session (stable across
        //    unrelated requests), so reusing its persisted conversation would
        //    wrongly continue a prior client's thread and drop tool context.
        let state = MiMoSessionState {
            conversation_id: upload::rand_hex(16),
            model: self.model_id.clone(),
            enable_thinking: default_thinking(&self.model_id),
            web_search_status: "disabled".to_string(),
        };
        self.store
            .insert(gateway_session_id.to_string(), state.clone())
            .await;
        state
    }

    /// Process attachments in the request into `multiMedias` entries.
    async fn process_attachments(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<Vec<MediaItem>, GatewayError> {
        let all_urls: Vec<String> = request
            .messages
            .iter()
            .flat_map(|m| m.content.image_urls())
            .chain(request.messages.iter().flat_map(|m| m.content.file_urls()))
            .collect();

        if all_urls.is_empty() {
            return Ok(Vec::new());
        }

        let client = build_reqwest_client()?;
        let wire = wire_model(&self.model_id).to_string();
        
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
            match download_and_hash_batch(&client, remote_urls).await {
                Ok(hashed_files) => {
                    // Upload concurrently too; the upload is the latency
                    // bottleneck. Order is preserved by collecting results by
                    // index and reassembling in original order.
                    use futures::stream::{self, StreamExt};
                    let mut results: Vec<(usize, Result<crate::providers::mimo::upload::MediaItem, String>)> =
                        Vec::with_capacity(hashed_files.len());
                    let mut buffered = stream::iter(hashed_files.into_iter().enumerate())
                        .map(|(idx, hashed)| {
                            let cookie_header = self.cookie_header.clone();
                            let ph = self.ph.clone();
                            let wire = wire.clone();
                            let upload_cache = self.upload_cache.clone();
                            let client = client.clone();
                            async move {
                                let result = upload::upload_hashed_file(
                                    &client,
                                    &cookie_header,
                                    &ph,
                                    &hashed.bytes,
                                    &hashed.name,
                                    &wire,
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
                            Ok(item) => processed.push(item),
                            Err(e) => {
                                tracing::warn!(error = %e, "MiMo concurrent upload failed, skipping");
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "MiMo concurrent download failed");
                }
            }
        }

        // Process data URIs sequentially
        for url in data_uris {
            match upload::resolve_url(
                &client,
                &self.cookie_header,
                &self.ph,
                &url,
                &wire,
                &self.upload_cache,
            )
            .await
            {
                Ok(item) => processed.push(item),
                Err(e) => {
                    tracing::warn!(url = %url, error = %e, "MiMo data URI upload failed, skipping attachment");
                }
            }
        }
        Ok(processed)
    }

    /// Build the multi-turn query string from the message history.
    ///
    /// MiMo's `query` is a single text field. On a fresh thread the full
    /// conversation is rendered with role labels (matching the web app's
    /// serialization); on a continued thread only the messages from the last
    /// user or tool turn onward are rendered because the server keeps the
    /// conversation state.
    fn build_query(&self, request: &ChatCompletionRequest, is_continuation: bool) -> String {
        let start = if is_continuation {
            request
                .messages
                .iter()
                .rposition(|m| m.role == "user" || m.role == "tool")
                .unwrap_or(0)
        } else {
            0
        };
        let messages = &request.messages[start..];

        // Collect assistant tool calls by id so tool-result messages can be
        // matched back to the call they answer (no native tool channel).
        let mut calls_by_id: std::collections::HashMap<&str, &crate::models::ToolCall> =
            std::collections::HashMap::new();
        for m in messages {
            if let Some(calls) = &m.tool_calls {
                for call in calls {
                    calls_by_id.insert(call.id.as_str(), call);
                }
            }
        }

        let mut parts: Vec<String> = Vec::new();
        for m in messages {
            let text = m.content.as_text();
            if m.role == "tool" {
                let call = m
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| calls_by_id.get(id).copied());
                let items: Vec<(Option<&crate::models::ToolCall>, Option<&str>, &str)> =
                    vec![(call, m.tool_call_id.as_deref(), &text)];
                let formatted = mtp::format_tool_results(&items);
                if !formatted.is_empty() {
                    parts.push(formatted);
                }
                continue;
            }
            if !text.is_empty() {
                let label = match m.role.as_str() {
                    "system" => "System",
                    "assistant" => "Assistant",
                    "user" => "Human",
                    _ => m.role.as_str(),
                };
                parts.push(format!("{}: {}", label, text));
            }
            // Render assistant tool calls into the transcript as MTP blocks
            // so the model sees the same dialect it is asked to emit.
            if let Some(calls) = &m.tool_calls {
                for call in calls {
                    let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
                    let mtp_call = mtp::MirageToolCall {
                        id: Some(call.id.clone()),
                        name: call.function.name.clone(),
                        arguments: args,
                    };
                    let block = serde_json::to_string(&mtp_call)
                        .unwrap_or_else(|_| "{}".to_string());
                    parts.push(format!(
                        "{}\n{}\n{}",
                        mtp::TOOL_CALL_START,
                        block,
                        mtp::TOOL_CALL_END
                    ));
                }
            }
        }
        let joined = parts.join("\n");
        // MiMo has no native function-calling channel (verified live). When
        // tools are requested, compile them into the MTP system prompt and
        // parse [MIRAGE_TOOL_CALL_V1] blocks out of the reply.
        if let Some(tools) = &request.tools {
            format!(
                "{}\n\nUser request:\n{}",
                mtp::build_mtp_system_prompt(tools, request.tool_choice.as_ref(), false),
                joined
            )
        } else {
            joined
        }
    }

    /// Non-streaming chat. Buffers the SSE stream internally.
    pub async fn chat(
        &self,
        request: ChatCompletionRequest,
        gateway_session_id: &str,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let response_id = format!("chatcmpl-{}", upload::rand_hex(16));
        let model = request.model.clone();

        let attachments = self.process_attachments(&request).await?;
        let mut state = self.resolve_conversation(gateway_session_id, &request.session_url).await;
        let is_continuation = request.session_url.is_some();
        let query = self.build_query(&request, is_continuation);

        // Apply per-request toggles onto the persisted state.
        state.enable_thinking = request.thinking.unwrap_or(state.enable_thinking);
        state.web_search_status = if request.search.unwrap_or(false) {
            "enabled".to_string()
        } else {
            "disabled".to_string()
        };
        state.model = self.model_id.clone();
        self.store.update(gateway_session_id, state.clone()).await;

        let mut stream = self
            .send_message_sse(&state, &request, &query, &attachments)
            .await?;
        let mut full_content = String::new();
        let mut full_thinking = String::new();
        let mut tool_calls: Vec<crate::models::ToolCall> = Vec::new();
        let mut finish_reason = "stop".to_string();
        let usage: Option<Usage> = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for choice in chunk.choices {
                if let Some(ref fr) = choice.finish_reason {
                    finish_reason = fr.clone();
                }
                if let Some(ref c) = choice.delta.content {
                    full_content.push_str(c);
                }
                if let Some(ref t) = choice.delta.reasoning_content {
                    full_thinking.push_str(t);
                }
                if let Some(ref calls) = choice.delta.tool_calls {
                    for call in calls {
                        if !tool_calls.iter().any(|c| c.id == call.id) {
                            tool_calls.push(call.clone());
                        }
                    }
                }
            }
        }

        let has_tool_calls = !tool_calls.is_empty();
        if has_tool_calls {
            finish_reason = "tool_calls".to_string();
        } else if finish_reason == "tool_calls" {
            finish_reason = "stop".to_string();
        }

        let prompt_text: String = request
            .messages
            .iter()
            .map(|m| m.content.as_text())
            .collect();
        let usage = usage.unwrap_or_else(|| estimate_cost(&prompt_text, &full_content));

        Ok(ChatCompletionResponse {
            id: response_id,
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model,
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: ChatContent::String(full_content),
                    name: None,
                    reasoning_content: if full_thinking.is_empty() {
                        None
                    } else {
                        Some(full_thinking)
                    },
                    citations: None,
                    tool_calls: if has_tool_calls { Some(tool_calls) } else { None },
                    tool_call_id: None,
                },
                finish_reason,
            }],
            usage,
            session_url: Some(format!("{}/chat/{}", API_HOST, state.conversation_id)),
        })
    }

    /// Streaming chat. Forwards MiMo SSE deltas as OpenAI chunks.
    pub async fn chat_stream(
        &self,
        request: ChatCompletionRequest,
        gateway_session_id: &str,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let response_id = format!("chatcmpl-{}", upload::rand_hex(16));
        let model_id = request.model.clone();
        let tools_enabled = request.tools.is_some();

        let attachments = self.process_attachments(&request).await?;
        let mut state = self.resolve_conversation(gateway_session_id, &request.session_url).await;
        let is_continuation = request.session_url.is_some();
        let query = self.build_query(&request, is_continuation);

        state.enable_thinking = request.thinking.unwrap_or(state.enable_thinking);
        state.web_search_status = if request.search.unwrap_or(false) {
            "enabled".to_string()
        } else {
            "disabled".to_string()
        };
        state.model = self.model_id.clone();
        let conv_id = state.conversation_id.clone();
        self.store.update(gateway_session_id, state.clone()).await;

        let raw_stream = self
            .send_message_sse(&state, &request, &query, &attachments)
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

        // Tool definitions for MTP validation inside the stream task.
        let tool_defs = request.tools.clone().unwrap_or_default();

        tokio::spawn({
            let tx = tx.clone();
            let response_id = response_id.clone();
            let model_id = model_id.clone();
            let session_url = Some(format!("{}/chat/{}", API_HOST, conv_id));
            async move {
                let mut role_emitted = false;
                let mut finish_emitted = false;
                let mut prev_content = String::new();
                let mut prev_thinking = String::new();
                // MTP stream state: absorbs [MIRAGE_TOOL_CALL_V1] blocks so
                // they never leak into streamed text, validates them against
                // the client definitions, and collects OpenAI-shaped calls.
                let mut mtp_state = mtp::MtpStreamState::new();
                let mut tool_calls: Vec<crate::models::ToolCall> = Vec::new();

                let mut stream = raw_stream;
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };

                    let mut delta_content: Option<String> = None;
                    let mut delta_thinking: Option<String> = None;
                    let mut fr: Option<String> = None;

                    for choice in chunk.choices {
                        if let Some(ref c) = choice.delta.content {
                            let new = if c.starts_with(&prev_content) {
                                c[prev_content.len()..].to_string()
                            } else {
                                c.clone()
                            };
                            prev_content = c.clone();
                            let clean = if tools_enabled {
                                let clean = mtp_state.process_delta(&new, &tool_defs);
                                if !mtp_state.collected_tool_calls.is_empty() {
                                    for tc in mtp_state.collected_tool_calls.drain(..) {
                                        if !tool_calls.iter().any(|c| c.id == tc.id) {
                                            tool_calls.push(tc);
                                        }
                                    }
                                }
                                clean
                            } else {
                                new
                            };
                            if !clean.is_empty() {
                                delta_content = Some(clean);
                            }
                        }
                        if let Some(ref t) = choice.delta.reasoning_content {
                            let new = if t.starts_with(&prev_thinking) {
                                t[prev_thinking.len()..].to_string()
                            } else {
                                t.clone()
                            };
                            if !new.is_empty() {
                                delta_thinking = Some(new);
                            }
                            prev_thinking = t.clone();
                        }
                        if let Some(ref f) = choice.finish_reason {
                            fr = Some(f.clone());
                            finish_emitted = true;
                        }
                    }

                    if delta_content.is_none()
                        && delta_thinking.is_none()
                        && fr.is_none()
                    {
                        continue;
                    }

                    let role = if !role_emitted {
                        role_emitted = true;
                        Some("assistant".to_string())
                    } else {
                        None
                    };

                    let _ = tx.send(Ok(ChatCompletionChunk {
                        id: response_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created: current_timestamp(),
                        model: model_id.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: ChatMessageDelta {
                                role,
                                content: delta_content,
                                reasoning_content: delta_thinking,
                                citations: None,
                                tool_calls: None,
                            },
                            finish_reason: fr,
                        }],
                        session_url: session_url.clone(),
                    }));
                }

                // Flush any pending MTP block at stream end, then emit
                // collected tool calls as a single final chunk.
                if tools_enabled {
                    mtp_state.finish(&tool_defs);
                    for tc in mtp_state.collected_tool_calls.drain(..) {
                        if !tool_calls.iter().any(|c| c.id == tc.id) {
                            tool_calls.push(tc);
                        }
                    }
                }
                if tools_enabled && !tool_calls.is_empty() {
                    let calls: Vec<crate::models::ToolCall> = tool_calls.clone();
                    let _ = tx.send(Ok(ChatCompletionChunk {
                        id: format!("{}-tools", response_id),
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
                                tool_calls: Some(calls),
                            },
                            finish_reason: Some("tool_calls".to_string()),
                        }],
                        session_url: session_url.clone(),
                    }));
                    finish_emitted = true;
                }

                if !finish_emitted {
                    let _ = tx.send(Ok(ChatCompletionChunk {
                        id: format!("{}-final", response_id),
                        object: "chat.completion.chunk".to_string(),
                        created: current_timestamp(),
                        model: model_id.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: ChatMessageDelta::default(),
                            finish_reason: Some("stop".to_string()),
                        }],
                        session_url: session_url.clone(),
                    }));
                }
            }
        });

        Ok(UnboundedReceiverStream::new(rx).boxed())
    }

    /// POST the message to the MiMo chat endpoint and parse the SSE stream
    /// into OpenAI chunks.
    async fn send_message_sse(
        &self,
        state: &MiMoSessionState,
        request: &ChatCompletionRequest,
        query: &str,
        attachments: &[MediaItem],
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let wire = wire_model(&self.model_id).to_string();
        let temperature = request.temperature.unwrap_or(0.8);
        let top_p = request.top_p.unwrap_or(0.95);

        let payload = serde_json::json!({
            "msgId": upload::rand_hex(16),
            "conversationId": state.conversation_id,
            "query": query,
            "isEditedQuery": false,
            "modelConfig": {
                "enableThinking": state.enable_thinking,
                "webSearchStatus": state.web_search_status,
                "model": wire,
                "temperature": temperature,
                "topP": top_p,
            },
            "multiMedias": attachments.iter().map(|m| serde_json::json!({
                "mediaType": m.media_type,
                "fileUrl": m.file_url,
                "name": m.name,
                "size": m.size,
                "status": m.status,
                "objectName": m.object_name,
                "url": m.url,
                "tokenUsage": m.token_usage,
            })).collect::<Vec<_>>(),
        });

        let body_str = serde_json::to_string(&payload)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;

        let url = format!("{}/open-apis/bot/chat?xiaomichatbot_ph={}", API_HOST, upload::urlencode(&self.ph));
        let client = build_reqwest_client()?;
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept-Language", "system")
            .header("x-timeZone", "Asia/Shanghai")
            .header("Cookie", &self.cookie_header)
            .body(body_str.clone())
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("MiMo chat request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp
                .text()
                .await
                .unwrap_or_else(|_| format!("HTTP {status}"));
            return Err(GatewayError::Provider(format!(
                "MiMo chat returned {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }

        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

        tokio::spawn({
            let tx = tx.clone();
            async move {
                let mut buf: Vec<u8> = Vec::with_capacity(4096);
                let mut http_chunks = resp.bytes_stream();
                let mut in_think = false;
                let mut usage: Option<Usage> = None;

                while let Some(chunk_result) = http_chunks.next().await {
                    let chunk = match chunk_result {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(GatewayError::Provider(format!(
                                "MiMo stream read error: {e}"
                            ))));
                            return;
                        }
                    };
                    buf.extend_from_slice(&chunk);

                    loop {
                        let boundary = match find_sse_boundary(&buf) {
                            Some(b) => b,
                            None => break,
                        };
                        let event = buf[..boundary].to_vec();
                        buf.drain(..boundary);
                        let text = match String::from_utf8(event) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        let mut event_type: Option<&str> = None;
                        let mut data: Option<&str> = None;
                        for line in text.lines() {
                            if let Some(rest) = line.strip_prefix("event:") {
                                event_type = Some(rest.trim());
                            } else if let Some(rest) = line.strip_prefix("data:") {
                                data = Some(rest.trim());
                            }
                        }
                        let event_type = event_type.unwrap_or("");
                        let Some(raw) = data else { continue };
                        if raw.is_empty() {
                            continue;
                        }

                        let current_event = if event_type == "message" || event_type.is_empty() {
                            "message"
                        } else {
                            event_type
                        };

                        match current_event {
                            "error" => {
                                let msg = serde_json::from_str::<serde_json::Value>(raw)
                                    .ok()
                                    .and_then(|v| {
                                        v.get("content")
                                            .and_then(|c| c.as_str())
                                            .map(|s| s.to_string())
                                    })
                                    .unwrap_or_else(|| raw.to_string());
                                let _ = tx.send(Err(GatewayError::Provider(format!(
                                    "MiMo error: {msg}"
                                ))));
                                return;
                            }
                            "usage" => {
                                if let Ok(u) = serde_json::from_str::<MiMoUsage>(raw) {
                                    usage = Some(Usage {
                                        prompt_tokens: u.prompt_tokens,
                                        completion_tokens: u.completion_tokens,
                                        total_tokens: u.total_tokens,
                                    });
                                }
                            }
                            "dialogId" => {
                                // Server echo of the conversation id; not
                                // user-visible content, skip it.
                                continue;
                            }
                            "finish" => {
                                // Final usage chunk, then done.
                                emit_usage(&tx, usage.as_ref());
                                let _ = tx.send(Ok(ChatCompletionChunk {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    object: "chat.completion.chunk".to_string(),
                                    created: current_timestamp(),
                                    model: String::new(),
                                    choices: vec![ChunkChoice {
                                        index: 0,
                                        delta: ChatMessageDelta::default(),
                                        finish_reason: Some("stop".to_string()),
                                    }],
                                    session_url: None,
                                }));
                                return;
                            }
                            _ => {
                                // "message" or any other event with content
                                let content: Option<String> = serde_json::from_str::<serde_json::Value>(raw)
                                    .ok()
                                    .and_then(|v| {
                                        v.get("content").and_then(|c| c.as_str()).map(|s| s.to_string())
                                    });
                                let Some(content) = content else { continue };
                                if content.is_empty() {
                                    continue;
                                }
                                let content = strip_control_tokens(&content);
                                let (text_chunks, think_chunks) =
                                    split_think_markers(&content, &mut in_think);
                                for t in text_chunks {
                                    if !t.is_empty() {
                                        let _ = tx.send(Ok(ChatCompletionChunk {
                                            id: uuid::Uuid::new_v4().to_string(),
                                            object: "chat.completion.chunk".to_string(),
                                            created: current_timestamp(),
                                            model: String::new(),
                                            choices: vec![ChunkChoice {
                                                index: 0,
                                                delta: ChatMessageDelta {
                                                    role: None,
                                                    content: Some(t),
                                                    reasoning_content: None,
                                                    citations: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: None,
                                            }],
                                            session_url: None,
                                        }));
                                    }
                                }
                                for t in think_chunks {
                                    if !t.is_empty() {
                                        let _ = tx.send(Ok(ChatCompletionChunk {
                                            id: uuid::Uuid::new_v4().to_string(),
                                            object: "chat.completion.chunk".to_string(),
                                            created: current_timestamp(),
                                            model: String::new(),
                                            choices: vec![ChunkChoice {
                                                index: 0,
                                                delta: ChatMessageDelta {
                                                    role: None,
                                                    content: None,
                                                    reasoning_content: Some(t),
                                                    citations: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: None,
                                            }],
                                            session_url: None,
                                        }));
                                    }
                                }
                            }
                        }
                    }
                }

                emit_usage(&tx, usage.as_ref());
            }
        });

        Ok(UnboundedReceiverStream::new(rx).boxed())
    }
}

fn emit_usage(
    tx: &mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
    usage: Option<&Usage>,
) {
    if let Some(u) = usage {
        let _ = tx.send(Ok(ChatCompletionChunk {
            id: uuid::Uuid::new_v4().to_string(),
            object: "chat.completion.chunk".to_string(),
            created: current_timestamp(),
            model: String::new(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChatMessageDelta::default(),
                finish_reason: None,
            }],
            session_url: None,
        }));
        let _ = u; // usage is surfaced on the non-streaming path; stream emits finish only
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct MiMoUsage {
    #[serde(default, alias = "promptTokens")]
    prompt_tokens: i32,
    #[serde(default, alias = "completionTokens")]
    completion_tokens: i32,
    #[serde(default, alias = "totalTokens")]
    total_tokens: i32,
}

/// Split a content delta on `<think>` / `</think>` markers into text and
/// reasoning fragments. The `in_think` flag tracks whether the marker pair
/// spans multiple SSE deltas.
///
/// The model emits `\u0000` (NUL) as a separator right after each marker
/// (`<think>\u0000...` and `</think>\u0000...`); strip it so it never leaks
/// into user-visible content.
fn split_think_markers(chunk: &str, in_think: &mut bool) -> (Vec<String>, Vec<String>) {
    let mut text = Vec::new();
    let mut think = Vec::new();
    let mut rest = chunk;

    loop {
        if *in_think {
            if let Some(i) = rest.find("</think>") {
                think.push(clean_think_fragment(&rest[..i]));
                rest = &rest[i + 8..];
                *in_think = false;
            } else {
                think.push(clean_think_fragment(rest));
                break;
            }
        } else {
            if let Some(i) = rest.find("<think>") {
                text.push(clean_think_fragment(&rest[..i]));
                rest = &rest[i + 7..];
                *in_think = true;
            } else {
                text.push(clean_think_fragment(rest));
                break;
            }
        }
    }

    (text, think)
}

/// Drop NUL separators (and any other control bytes) from a fragment.
fn clean_think_fragment(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Strip MiMo control tokens from a content delta.
///
/// The model emits `webSearch` as a control signal when it decides to run a
/// web search (regardless of the request `search` toggle). It is not
/// user-visible text, so it must never leak into content.
fn strip_control_tokens(s: &str) -> String {
    let mut out = s.to_string();
    for token in ["webSearch"] {
        loop {
            let prev = out.clone();
            out = out
                .replace(&format!("{token}\u{0}"), "")
                .replace(&format!("{token} "), "")
                .replace(&format!(" {token}"), "");
            if let Some(rest) = out.strip_prefix(token) {
                out = rest.to_string();
            }
            if out == prev {
                break;
            }
        }
    }
    out
}

/// Extract a conversation id from a `session_url` of the form
/// `https://aistudio.xiaomimimo.com/chat/{conversationId}`.
fn extract_conversation_id(url: &str) -> Option<String> {
    let path = url.strip_prefix(&format!("{}/chat/", API_HOST))?;
    if path.is_empty() || path.contains('/') {
        return None;
    }
    Some(path.to_string())
}

fn estimate_cost(prompt: &str, completion: &str) -> Usage {
    let prompt_tokens = (prompt.len() / 4).max(1) as i32;
    let completion_tokens = (completion.len() / 4).max(1) as i32;
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    }
}

fn find_sse_boundary(buf: &[u8]) -> Option<usize> {
    let len = buf.len();
    if len < 2 {
        return None;
    }
    if len >= 4 {
        let mut i = 0;
        while i <= len - 4 {
            if buf[i] == b'\r' && buf[i + 1] == b'\n' && buf[i + 2] == b'\r' && buf[i + 3] == b'\n'
            {
                return Some(i + 4);
            }
            i += 1;
        }
    }
    let mut i = 0;
    while i <= len - 2 {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_control_tokens_removes_websearch_prefix() {
        assert_eq!(strip_control_tokens("webSearch"), "");
        assert_eq!(strip_control_tokens("webSearchHello"), "Hello");
        assert_eq!(
            strip_control_tokens("webSearchBased on the data"),
            "Based on the data"
        );
    }

    #[test]
    fn strip_control_tokens_keeps_plain_text() {
        assert_eq!(strip_control_tokens("Hello world"), "Hello world");
        assert_eq!(strip_control_tokens(""), "");
    }
}
