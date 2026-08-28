use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures::stream::{BoxStream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info};
use url::Url;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatMessageDelta, ChunkChoice, Citation, Tool, ToolCall, Usage,
};
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::tool_call::format_tool_results;
use crate::providers::send_with_retry;
use crate::session::{SessionHandle, SessionManager};

use super::auth::{
    build_request_headers, extract_auth_data, navigate_to_gemini, AuthData,
};
use super::models::{resolve_model, GeminiModelDef};
use super::rpc::{
    build_f_req, build_request_payload, clean_response_text, extract_conversation_from_line,
    parse_full_response, parse_streaming_chunk, stream_generate_url, ConversationState,
};
use super::state::SessionStore;
use super::upload::{
    build_file_list, decode_data_uri, derive_filename, upload_files,
};
use crate::providers::streaming_upload::download_and_hash_batch;

const GEMINI_BASE_URL: &str = "https://gemini.google.com";
const TIMEOUT_SECS: u64 = 120;

/// Generate a new session token.
fn new_session_token() -> String {
    format!("gem_{}", uuid::Uuid::new_v4().simple())
}

/// Format: `https://gemini.google.com/app#session=<token>`
fn make_session_url(token: &str) -> String {
    format!("{}/app#session={}", GEMINI_BASE_URL, token)
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

/// Direct API client for Gemini's internal StreamGenerate endpoint.
pub struct GeminiDirectClient {
    http: reqwest::Client,
    auth: AuthData,
    req_id: AtomicU64,
    conversation: Option<ConversationState>,
    model: &'static GeminiModelDef,
    store: SessionStore,
    /// Token for the current session (for storing conversation updates).
    session_token: Option<String>,
}

impl GeminiDirectClient {
    pub async fn new(
        session: &SessionHandle,
        model_id: &str,
        sessions: SessionManager,
        _prev_conversation: Option<ConversationState>,
        store: SessionStore,
    ) -> Result<Self, GatewayError> {
        let model = resolve_model(model_id).ok_or_else(|| {
            GatewayError::BadRequest(format!("unknown Gemini model: {model_id}"))
        })?;

        let auth = match extract_auth_data(&sessions, &session.id).await {
            Ok(data) => data,
            Err(_) => {
                navigate_to_gemini(&sessions, &session.id).await?;
                extract_auth_data(&sessions, &session.id).await?
            }
        };

        let gemini_url = Url::parse(GEMINI_BASE_URL)
            .map_err(|e| GatewayError::Internal(format!("invalid URL: {e}")))?;
        let cookie_header = session.cookie_jar.get_cookie_header(&gemini_url);

        debug!(
            cookie_count = cookie_header.split(';').count(),
            "Gemini auth: cookie header stats"
        );

        let headers = build_request_headers(
            &cookie_header,
            &session.user_agent,
            model.model_header,
        )?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .default_headers(headers)
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;

        info!(
            session_id = %session.id,
            model = %model_id,
            "Gemini DirectClient initialized"
        );

        Ok(Self {
            http,
            auth,
            req_id: AtomicU64::new(1),
            conversation: _prev_conversation,
            model,
            store,
            session_token: None,
        })
    }

    /// Build the query parameters and form body for a StreamGenerate request.
    fn build_request(
        &self,
        prompt: &str,
        tools: Option<&[Tool]>,
        image_list: Option<&serde_json::Value>,
        search: bool,
    ) -> Result<(Vec<(&'static str, String)>, Vec<(&'static str, String)>), GatewayError> {
        let req_id = self.req_id.fetch_add(1, Ordering::Relaxed);
        let _reqid = format!("{}", 1000 + req_id);

        let request_uuid = uuid::Uuid::new_v4().to_string();

        let payload = build_request_payload(
            prompt,
            self.conversation.as_ref(),
            tools,
            image_list,
            4, // default think_mode (shallow thinking = fast)
            self.model.mode,
            &request_uuid,
            search,
        )
        .map_err(|e| GatewayError::Internal(format!("failed to build Gemini request payload: {e}")))?;
        let f_req = build_f_req(&payload)
            .map_err(|e| GatewayError::Internal(format!("failed to build Gemini f.req: {e}")))?;

        let query = vec![
            ("bl", self.auth.bl.clone()),
            ("hl", "en".to_string()),
            ("_reqid", _reqid),
            ("rt", "c".to_string()),
            ("f.sid", self.auth.sid.clone()),
        ];

        let form = vec![
            ("at", self.auth.snlm0e.clone()),
            ("f.req", f_req),
        ];

        Ok((query, form))
    }

    /// Resolve conversation state from a session URL.
    /// Returns the session token and conversation state, or None for new conversations.
    async fn resolve_conversation(
        &self,
        session_url: &Option<String>,
    ) -> Result<Option<(String, ConversationState)>, GatewayError> {
        let token = match session_url.as_ref().and_then(|u| extract_session_token(u)) {
            Some(t) => t,
            None => return Ok(None),
        };

        match self.store.acquire(&token).await {
            Some(stored) => {
                Ok(Some((token, ConversationState {
                    conversation_id: stored.conversation_id,
                    response_id: stored.response_id,
                    choice_id: stored.choice_id,
                })))
            }
            None => Err(GatewayError::BadRequest(format!(
                "invalid or expired session_url: {}",
                session_url.as_deref().unwrap_or("")
            ))),
        }
    }

    /// Store conversation state. Reuses existing token if available so the
    /// returned session_url keeps continuing the same linear thread.
    ///
    /// Gemini's web client (the Bard batchexecute protocol) treats each chat
    /// as a single linear message list; there is no tree-branch or
    /// fork-from-here flow in its RPC surface. Continuations serialize on one
    /// shared tip, which is the same behaviour the web app exhibits. No lock
    /// is held; the shared store is the single source of truth and the reused
    /// token is the continuity handle.
    async fn store_conversation(
        &self,
        conv: &ConversationState,
        existing_token: Option<&str>,
    ) -> String {
        let token = existing_token
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .unwrap_or_else(new_session_token);

        // FIX: insert() creates the TrackedSession before set_data().
        self.store
            .insert(token.clone(), conv, self.model.id)
            .await;
        make_session_url(&token)
    }

    /// Send a BARD_ACTIVITY RPC to the Gemini backend.
    ///
    /// This is a batchexecute call with `rpcids=ESY5D` that signals the user
    /// is actively attaching media. The real web UI sends this before any
    /// StreamGenerate request that includes images; without it the backend
    /// may silently ignore attached file URLs.
    async fn send_bard_activity(&self) -> Result<(), GatewayError> {
        let req_id = self.req_id.fetch_add(1, Ordering::Relaxed);
        let _reqid = format!("{}", 1000 + req_id);

        let url = format!(
            "{}/_/BardChatUi/data/batchexecute?rpcids=ESY5D&bl={}&hl=en&_reqid={}&rt=c",
            GEMINI_BASE_URL, self.auth.bl, _reqid
        );

        // Inner payload: `[["bard_activity_enabled"]]`
        let inner = r#"[[["bard_activity_enabled"]]]"#;
        let f_req = serde_json::to_string(&vec![
            serde_json::Value::Null,
            serde_json::Value::String(inner.to_string()),
        ])
        .map_err(|e| GatewayError::Internal(format!("failed to serialize f.req: {e}")))?;

        let builder = self
            .http
            .post(&url)
            .form(&[
                ("at", self.auth.snlm0e.as_str()),
                ("f.req", f_req.as_str()),
            ]);

        match send_with_retry(builder).await {
            Ok(resp) if resp.status().is_success() => {
                debug!("Gemini bard_activity RPC succeeded");
                Ok(())
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    status = %status,
                    body = %body.chars().take(200).collect::<String>(),
                    "Gemini bard_activity RPC failed (non-fatal)"
                );
                Ok(()) // non-fatal
            }
            Err(e) => {
                tracing::warn!(error = %e, "Gemini bard_activity RPC error (non-fatal)");
                Ok(()) // non-fatal
            }
        }
    }

    /// Process file and image content parts from the last user message.
    ///
    /// Resolves all `image_url` and `file_url` references in the final message:
    /// decodes data URIs, downloads remote URLs (with SSRF protection), checks
    /// the upload cache, uploads new content to Google's content-push service,
    /// and returns the Gemini payload element `[[url, 1], name]` list.
    async fn process_message_attachments(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<Option<serde_json::Value>, GatewayError> {
        let last_msg = match request.messages.last() {
            Some(m) => m,
            None => return Ok(None),
        };
        let image_urls: Vec<String> = last_msg.content.image_urls();
        let file_urls: Vec<String> = last_msg.content.file_urls();

        if image_urls.is_empty() && file_urls.is_empty() {
            return Ok(None);
        }

        let cache = &self.store.upload_cache;
        let mut resolved_urls: Vec<(String, String, String)> = Vec::new();
        let mut pending: Vec<(Vec<u8>, String, String)> = Vec::new();

        // Process data URIs first (no download needed)
        for url in image_urls.iter().chain(file_urls.iter()) {
            if let Some((data, mime)) = decode_data_uri(url) {
                let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));
                let name = derive_filename(url, &mime);

                if let Some(cached_url) = cache.get(&hash).await {
                    resolved_urls.push((cached_url, name, mime));
                } else {
                    pending.push((data, name, mime));
                }
            }
        }

        // Collect remote URLs
        let remote_urls: Vec<String> = image_urls
            .iter()
            .chain(file_urls.iter())
            .filter(|u| !u.starts_with("data:"))
            .cloned()
            .collect();

        // Concurrently download and hash remote URLs
        if !remote_urls.is_empty() {
            match download_and_hash_batch(&self.http, remote_urls).await {
                Ok(hashed_files) => {
                    for hashed in hashed_files {
                        if let Some(cached_url) = cache.get(&hashed.hash_base64).await {
                            resolved_urls.push((cached_url, hashed.name, hashed.mime_type));
                        } else {
                            pending.push((hashed.bytes, hashed.name, hashed.mime_type));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "concurrent attachment download failed");
                }
            }
        }

        // Upload pending files
        if !pending.is_empty() {
            match upload_files(&self.http, &pending).await {
                Ok(upload_results) => {
                    for (uploaded_url, name) in upload_results.iter() {
                        // Store in cache by filename for subsequent requests
                        cache.insert(name.clone(), uploaded_url.clone()).await;
                        if let Some((_, _, mime_type)) = pending.iter().find(|(_, n, _)| n == name) {
                            resolved_urls.push((uploaded_url.clone(), name.clone(), mime_type.clone()));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "attachment upload failed");
                }
            }
        }

        let file_list = build_file_list(&resolved_urls);
        Ok(Some(file_list))
    }

    /// Handle tool-result messages: look up stored tool calls and format results.
    async fn handle_tool_results(
        &self,
        request: &ChatCompletionRequest,
        session_token: Option<&str>,
    ) -> Option<String> {
        let tool_msgs: Vec<&ChatMessage> = request
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .collect();

        if tool_msgs.is_empty() {
            return None;
        }

        let token = session_token?;

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
            None
        } else {
            Some(formatted)
        }
    }

    /// Build the final prompt, handling tool results injection and tool definitions.
    async fn build_prompt(
        &self,
        request: &ChatCompletionRequest,
        session_token: Option<&str>,
    ) -> (String, Option<Vec<Tool>>) {
        let last_msg = request.messages.last();
        let prompt = last_msg.map(|m| m.content.as_text()).unwrap_or_default();

        // Handle tool-result messages from previous turns
        let tool_result_prompt = self
            .handle_tool_results(request, session_token)
            .await
            .unwrap_or_default();

        let base_prompt = if tool_result_prompt.is_empty() {
            prompt
        } else {
            format!("{tool_result_prompt}\n\n{prompt}")
        };

        // Compile client tools into the MTP system prompt (universal
        // dialect). Native tool schemas are NOT forwarded upstream: the
        // gateway never sends OpenAI tools to the provider.
        if let Some(tools) = &request.tools {
            (
                format!(
                    "{}\n\n=== USER REQUEST (execute this task now) ===\n{}\n=== END USER REQUEST ===",
                    crate::providers::mtp::build_mtp_system_prompt(
                        tools,
                        request.tool_choice.as_ref(),
                        false,
                        crate::providers::mtp::prompt_style_for_model(&request.model)
                    ),
                    base_prompt
                ),
                Some(tools.clone()),
            )
        } else {
            (base_prompt, None)
        }
    }

    /// Non-streaming chat completion.
    pub async fn chat(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        // Resolve conversation from session_url.
        let conv_token = if let Some((token, conv)) =
            self.resolve_conversation(&request.session_url).await?
        {
            // Validate model match
            let stored = self.store.acquire(&token).await;
            if let Some(ref stored_conv) = stored {
                SessionStore::ensure_model_matches(&request.model, &stored_conv.model_id)?;
            }
            self.conversation = Some(conv);
            self.session_token = Some(token.clone());
            Some(token)
        } else {
            None
        };

        let (final_prompt, tool_refs) = self.build_prompt(&request, conv_token.as_deref()).await;

        let image_list = self.process_message_attachments(&request).await?;

        // Send BARD_ACTIVITY RPC before the first StreamGenerate request when
        // files are attached, so the backend binds the file references properly.
        if image_list.is_some() {
            let _ = self.send_bard_activity().await;
        }

        let (query, form) = self.build_request(&final_prompt, tool_refs.as_deref(), image_list.as_ref(), request.search.unwrap_or(false))?;

        debug!(
            model = %self.model.id,
            prompt_len = final_prompt.len(),
            search = %request.search.unwrap_or(false),
            "Sending Gemini StreamGenerate request"
        );

        let url = format!("{}{}", GEMINI_BASE_URL, stream_generate_url());
        let builder = self.http.post(&url).query(&query).form(&form);
        let resp = send_with_retry(builder)
            .await
            .map_err(|e| GatewayError::Provider(format!("Gemini request failed: {e}")))?;

        let status = resp.status();
        let body_text = resp
            .text()
            .await
            .map_err(|e| GatewayError::Provider(format!("Gemini response read failed: {e}")))?;

        if !status.is_success() {
            return Err(GatewayError::Provider(format!(
                "Gemini API returned {status}: {}",
                body_text.chars().take(200).collect::<String>()
            )));
        }

        let response_data = parse_full_response(&body_text)?;

        // Store conversation state for multi-turn support.
        // Reuse existing token for continuation, create new for first turn.
        let session_url = if let Some(ref conv) = response_data.conversation {
            self.conversation = Some(conv.clone());
            Some(self.store_conversation(conv, conv_token.as_deref()).await)
        } else {
            self.conversation = None;
            None
        };

        // Store tool calls for future tool-result handling.
        if let Some(ref calls) = response_data.tool_calls {
            if !calls.is_empty() {
                if let Some(ref url) = session_url {
                    if let Some(token) = extract_session_token(url) {
                        self.store.store_tool_calls(&token, calls).await;
                    }
                }
            }
        }

        // Clean tool call markers and citation markers from text.
        let clean_text = clean_response_text(&response_data.text);
        let tool_calls = response_data.tool_calls;
        let has_tool_calls = tool_calls.as_ref().map_or(false, |c| !c.is_empty());
        let citations = response_data.citations;
        let prompt_text: String = request.messages.iter().map(|m| m.content.as_text()).collect();
        let completion_text = format!("{}{}", response_data.text, response_data.thinking.as_deref().unwrap_or_default());

        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: self.model.id.to_string(),
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: crate::models::ChatContent::String(clean_text),
                    name: None,
                    reasoning_content: response_data.thinking,
                    citations,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason: if has_tool_calls {
                    "tool_calls".to_string()
                } else {
                    "stop".to_string()
                },
            }],
            usage: Usage {
                prompt_tokens: estimate_tokens("gemini", &self.model.id.to_string(), &prompt_text),
                completion_tokens: estimate_tokens("gemini", &self.model.id.to_string(), &completion_text),
                total_tokens: estimate_tokens("gemini", &self.model.id.to_string(), &prompt_text)
                    + estimate_tokens("gemini", &self.model.id.to_string(), &completion_text),
            },
            session_url,
        })
    }

    /// Streaming chat completion.
    pub async fn chat_stream(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        // Resolve conversation from session_url (no lock held).
        let conv_token: Option<String> =
            if let Some((token, conv)) = self.resolve_conversation(&request.session_url).await? {
                let stored = self.store.acquire(&token).await;
                if let Some(ref stored_conv) = stored {
                    SessionStore::ensure_model_matches(&request.model, &stored_conv.model_id)?;
                }
                self.conversation = Some(conv);
                self.session_token = Some(token.clone());
                Some(token)
            } else {
                None
            };

        let (final_prompt, tool_refs) = self.build_prompt(&request, conv_token.as_deref()).await;

        let image_list = self.process_message_attachments(&request).await?;

        let (query, form) = self.build_request(&final_prompt, tool_refs.as_deref(), image_list.as_ref(), request.search.unwrap_or(false))?;
        let url = format!("{}{}", GEMINI_BASE_URL, stream_generate_url());
        let model_id = self.model.id.to_string();
        let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());

        let http_client = self.http.clone();
        let store = self.store.clone();
        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();
        let counter = std::sync::atomic::AtomicU32::new(0);
        let mut session_token = conv_token.clone();

        // Send BARD_ACTIVITY RPC before the first StreamGenerate request when
        // files are attached, so the backend binds the file references properly.
        if image_list.is_some() {
            let _ = self.send_bard_activity().await;
        }

        tokio::spawn(async move {
            let builder = http_client.post(&url).query(&query).form(&form);
            let resp = match send_with_retry(builder).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(GatewayError::Provider(format!(
                        "Gemini streaming request failed: {e}"
                    ))));
                    return;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let _ = tx.send(Err(GatewayError::Provider(format!(
                    "Gemini API returned {status}: {}",
                    body.chars().take(200).collect::<String>()
                ))));
                return;
            }

            let mut stream = resp.bytes_stream();
            let mut buffer = Vec::new();
            let mut emitted_role = false;
            // Gemini re-emits cumulative snapshots; deltas are computed
            // against this and it advances with every accepted chunk.
            let mut previous_text = String::new();
            let mut last_conversation: Option<ConversationState> = None;
            let mut collected_tool_calls: Vec<ToolCall> = Vec::new();
            let mut session_url: Option<String> = None;

            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(GatewayError::Provider(format!(
                            "Gemini streaming read error: {e}"
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

                                        if let Some(conv) = extract_conversation_from_line(line) {
                                last_conversation = Some(conv.clone());
                                // First time we see conversation state → construct session URL
                                // for propagation in this and subsequent chunks.
                                if session_url.is_none() {
                                    let token = session_token
                                        .clone()
                                        .filter(|t| !t.is_empty())
                                        .unwrap_or_else(new_session_token);
                                    store.insert(token.clone(), &conv, &model_id).await;
                                    session_token = Some(token.clone());
                                    session_url = Some(make_session_url(&token));
                                }
                            }

                        match parse_streaming_chunk(line, &previous_text) {
                            Ok(Some((delta, thinking, tool_calls, citations, full_text))) => {
                                let role = if !emitted_role {
                                    emitted_role = true;
                                    Some("assistant".to_string())
                                } else {
                                    None
                                };

                                // Advance the dedupe cursor to the full
                                // cumulative snapshot.
                                previous_text = full_text;

                                if !build_streaming_chunk(
                                    &tx, &counter, &id_prefix, &model_id,
                                    delta, thinking, tool_calls, citations,
                                    role,
                                    &mut collected_tool_calls,
                                    session_url.as_deref(),
                                ) {
                                    return;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                if tx.send(Err(e)).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }

                buffer.drain(0..consumed);
            }

            // Process any remaining partial line.
            if !buffer.is_empty() {
                let line = String::from_utf8_lossy(&buffer);
                if let Some(conv) = extract_conversation_from_line(&line) {
                    last_conversation = Some(conv.clone());
                    if session_url.is_none() {
                        let token = session_token
                            .clone()
                            .filter(|t| !t.is_empty())
                            .unwrap_or_else(new_session_token);
                        store.insert(token.clone(), &conv, &model_id).await;
                        session_token = Some(token.clone());
                        session_url = Some(make_session_url(&token));
                    }
                }
                match parse_streaming_chunk(&line, &previous_text) {
                    Ok(Some((delta, thinking, tool_calls, citations, full_text))) => {
                        // Final partial line at stream end: no further dedupe
                        // cursor to advance.
                        let _ = full_text;
                        build_streaming_chunk(
                            &tx, &counter, &id_prefix, &model_id,
                            delta, thinking, tool_calls, citations,
                            None,
                            &mut collected_tool_calls,
                            session_url.as_deref(),
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }

            // Ensure the conversation is stored for the final session_url.
            // If session_url was already set by an intermediate chunk, reuse it.
            if let Some(ref conv) = last_conversation {
                let token = session_token
                    .clone()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(new_session_token);
                store.insert(token.clone(), conv, &model_id).await;
                if !collected_tool_calls.is_empty() {
                    store.store_tool_calls(&token, &collected_tool_calls).await;
                }
                if session_url.is_none() {
                    session_url = Some(make_session_url(&token));
                }
            }
            let final_session_url = session_url.clone();

            // Final chunk with stop/tool_calls signal and session_url.
            let final_finish_reason = if !collected_tool_calls.is_empty() {
                "tool_calls"
            } else {
                "stop"
            };
            counter.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(Ok(ChatCompletionChunk {
                id: format!("{}-final", id_prefix),
                object: "chat.completion.chunk".to_string(),
                created: current_timestamp(),
                model: model_id.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChatMessageDelta::default(),
                    finish_reason: Some(final_finish_reason.to_string()),
                }],
                session_url: final_session_url,
            }));
        });

        Ok(UnboundedReceiverStream::new(rx).boxed())
    }
}

/// Build and emit a single streaming chunk.
///
/// Returns `true` if the chunk was sent successfully (channel is still open),
/// or `false` if the receiver has dropped (caller should exit the producer).
///
/// This is the single point of construction for all streaming chunks,
/// ensuring consistent field population across the main loop and the
/// trailing-partial-line handler.
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
    collected_tool_calls: &mut Vec<ToolCall>,
    session_url: Option<&str>,
) -> bool {
    counter.fetch_add(1, Ordering::Relaxed);

    let has_tool_calls = tool_calls_raw.is_some();
    let new_tool_calls = tool_calls_raw
        .map(|calls| {
            let mut new_calls: Vec<ToolCall> = Vec::new();
            for call in calls {
                if !collected_tool_calls.iter().any(|c| c.id == call.id) {
                    new_calls.push(call.clone());
                    collected_tool_calls.push(call);
                }
            }
            new_calls
        })
        .filter(|c: &Vec<ToolCall>| !c.is_empty());

    let clean_delta = if has_tool_calls {
        clean_response_text(&delta)
    } else {
        delta.clone()
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

    tx.send(Ok(chunk)).is_ok()
}
