use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::info;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatMessageDelta, ChunkChoice, Citation, ToolCall, Usage,
};
use crate::providers::mtp;
use crate::providers::send_with_retry;
use crate::session::SessionHandle;

use super::auth::{
    build_request_headers, discover_org_id, extract_from_import, AuthData, CLAUDE_URL,
};
use super::models::{resolve_model, ClaudeModelDef};
use super::rpc::{
    build_native_request_payload, collect_text_from_sse,
    extract_usage_from_sse, parse_sse_line,
};
use super::state::{SessionStore, StoredConversation};
use super::upload::{decode_data_uri, derive_filename, upload_files};
use crate::providers::streaming_upload::download_and_hash_batch;

const TIMEOUT_SECS: u64 = 120;

fn new_session_token() -> String {
    format!("claude_{}", uuid::Uuid::new_v4().simple())
}

fn make_session_url(token: &str) -> String {
    format!("claude://session/{}", token)
}

fn extract_session_token(url: &str) -> Option<String> {
    url.strip_prefix("claude://session/").map(|s| s.to_string())
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub struct ClaudeDirectClient {
    http: Client,
    auth: AuthData,
    store: SessionStore,
    model_id: String,
    #[allow(dead_code)]
    model_def: ClaudeModelDef,
    session_token: Option<String>,
}

impl ClaudeDirectClient {
    pub async fn new(
        session: &SessionHandle,
        model_id: &str,
        store: SessionStore,
    ) -> Result<Self, GatewayError> {
        let model_def = resolve_model(model_id).ok_or_else(|| {
            GatewayError::BadRequest(format!("unknown Claude model: {model_id}"))
        })?;

        // Native auth only: the claude.ai internal API is authenticated with
        // the user's own session cookies. There is no anonymous mode and no
        // third-party proxy fallback. Without a valid `sessionKey` the
        // provider fails closed.
        let mut auth = extract_from_import(session.cookie_jar.as_ref()).ok_or_else(|| {
            GatewayError::Auth(
                "Claude session not found in the imported browser profile. \
                 Log in to https://claude.ai in the source browser and re-import \
                 the profile (requires the `sessionKey` cookie)."
                    .to_string(),
            )
        })?;

        if auth.org_id.is_empty() {
            match discover_org_id(&auth).await {
                Ok(org_id) => {
                    auth.org_id = org_id.clone();
                    auth.cookie_header = format!(
                        "sessionKey={}; lastActiveOrg={}; cf_clearance={}",
                        auth.session_key, org_id, auth.cf_clearance
                    );
                    info!("Claude org_id discovered; using direct mode");
                }
                Err(e) => {
                    return Err(GatewayError::Auth(format!(
                        "Claude org_id discovery failed: {e}. \
                         Log in to https://claude.ai in the source browser and re-import the profile."
                    )));
                }
            }
        }

        let headers = build_request_headers(&auth)?;

        let http = Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .default_headers(headers)
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;

        info!(
            session_id = %session.id,
            model = %model_id,
            org_id = %auth.org_id,
            "Claude DirectClient initialized"
        );

        Ok(Self {
            http,
            auth,
            store,
            model_id: model_def.id.to_string(),
            model_def,
            session_token: None,
        })
    }

    fn api_base_url(&self, org_id: &str) -> String {
        format!("{}/api/organizations/{}", CLAUDE_URL, org_id)
    }

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

    /// Store conversation state. Reuses the existing token so the returned
    /// session_url keeps continuing the same linear thread.
    ///
    /// Claude's web client has no message-tree or fork-from-here model: a
    /// conversation is a single linear message chain keyed by a conversation
    /// id, and there is no endpoint to branch from a specific prior message.
    /// Continuations therefore serialize on one shared tip, which is the same
    /// behaviour the official web app exhibits. No lock is held; the shared
    /// store is the single source of truth and the reused token is the
    /// continuity handle.
    async fn store_conversation(
        &self,
        conv: &StoredConversation,
        existing_token: Option<&str>,
    ) -> String {
        let token = existing_token
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .unwrap_or_else(new_session_token);
        self.store.insert(token.clone(), conv, &self.model_id).await;
        make_session_url(&token)
    }

    fn detect_timezone() -> String {
        String::from("Etc/UTC")
    }

    async fn send_stream_request(
        &self,
        request: &ChatCompletionRequest,
        tool_prompt: &str,
        org_id: &str,
        search: bool,
        file_refs: &[String],
        image_refs: &[String],
    ) -> Result<reqwest::Response, GatewayError> {
        let mut msgs = request.messages.clone();

        if !tool_prompt.is_empty() {
            if let Some(last) = msgs.last_mut() {
                if last.role == "user" {
                    let original = last.content.as_text();
                    last.content = crate::models::ChatContent::String(
                        format!("{}{}", tool_prompt, original),
                    );
                }
            }
        }

        let timezone = Self::detect_timezone();
        let base = self.api_base_url(org_id);

        let payload = build_native_request_payload(request, &timezone, search, file_refs, image_refs);
        let url = format!("{}/chat_conversations", base);

        let req_builder = self.http.post(&url).json(&payload);

        let resp = req_builder
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("Claude request failed: {e}")))?;

        Ok(resp)
    }

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

        let formatted = mtp::format_tool_results(&refs);
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

    /// Process file and image attachments from the last user message.
    /// Returns (file_refs, image_refs) for the native payload.
    async fn process_message_attachments(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<(Vec<String>, Vec<String>), GatewayError> {
        let last_msg = match request.messages.last() {
            Some(m) => m,
            None => return Ok((Vec::new(), Vec::new())),
        };
        let image_urls: Vec<String> = last_msg.content.image_urls();
        let file_urls: Vec<String> = last_msg.content.file_urls();

        if image_urls.is_empty() && file_urls.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let cache = &self.store.upload_cache;
        let mut resolved_file_refs: Vec<String> = Vec::new();
        let mut resolved_image_refs: Vec<String> = Vec::new();
        let mut pending: Vec<(Vec<u8>, String)> = Vec::new();

        // Process data URIs first (no download needed)
        for url in image_urls.iter() {
            if let Some((data, mime)) = decode_data_uri(url) {
                let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));
                let name = derive_filename(url, &mime);

                if let Some(cached) = cache.get(&hash).await {
                    resolved_image_refs.push(cached);
                } else {
                    pending.push((data, name));
                }
            }
        }

        for url in file_urls.iter() {
            if let Some((data, mime)) = decode_data_uri(url) {
                let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&data));
                let name = derive_filename(url, &mime);

                if let Some(cached) = cache.get(&hash).await {
                    resolved_file_refs.push(cached);
                } else {
                    pending.push((data, name));
                }
            }
        }

        // Collect remote URLs (both images and files)
        let mut remote_urls = Vec::new();
        let mut remote_is_image = Vec::new();

        for url in image_urls.iter() {
            if !url.starts_with("data:") {
                remote_urls.push(url.clone());
                remote_is_image.push(true);
            }
        }

        for url in file_urls.iter() {
            if !url.starts_with("data:") {
                remote_urls.push(url.clone());
                remote_is_image.push(false);
            }
        }

        // Concurrently download and hash remote URLs
        if !remote_urls.is_empty() {
            match download_and_hash_batch(&self.http, remote_urls.clone()).await {
                Ok(hashed_files) => {
                    for (hashed, is_image) in hashed_files.into_iter().zip(remote_is_image) {
                        let hash_hex = hashed.hash_base64; // Already in base64

                        if let Some(cached) = cache.get(&hash_hex).await {
                            if is_image {
                                resolved_image_refs.push(cached);
                            } else {
                                resolved_file_refs.push(cached);
                            }
                        } else {
                            pending.push((hashed.bytes, hashed.name));
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
            match upload_files(&self.http, &pending, cache).await {
                Ok(ids) => {
                    resolved_file_refs.extend(ids);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "attachment upload failed");
                }
            }
        }

        Ok((resolved_file_refs, resolved_image_refs))
    }

    pub async fn chat(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let (conv_token, stored_conv) =
            match self.resolve_conversation(&request.session_url).await? {
                Some((token, stored)) => {
                    self.session_token = Some(token.clone());
                    (Some(token), Some(stored))
                }
                None => (None, None),
            };

        let stored_org_id = stored_conv.as_ref().map(|c| c.org_id.as_str()).unwrap_or(&self.auth.org_id).to_string();

        let last_msg = request.messages.last();
        let prompt = last_msg.map(|m| m.content.as_text()).unwrap_or_default();

        let tool_messages = self
            .handle_tool_results(&request, conv_token.as_deref())
            .await;

        let mut messages = request.messages.clone();
        if !tool_messages.is_empty() {
            messages.extend(tool_messages);
        }

        let tool_prompt = match request.tools.as_ref() {
            Some(tools) => format!(
                "{}\n\nUser request:\n{}",
                mtp::build_mtp_system_prompt(tools, request.tool_choice.as_ref(), false),
                prompt
            ),
            None => String::new(),
        };

        let search = request.search.unwrap_or(false);
        let (file_refs, image_refs) = self.process_message_attachments(&request).await?;

        let resp = self
            .send_stream_request(&request, &tool_prompt, &stored_org_id, search, &file_refs, &image_refs)
            .await?;

        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| GatewayError::Provider(format!("response read failed: {e}")))?;

        if !status.is_success() {
            return Err(GatewayError::Provider(format!(
                "Claude API returned {status}: {}",
                String::from_utf8_lossy(&body).chars().take(200).collect::<String>()
            )));
        }

        let (text, reasoning, citations, native_tool_calls) = collect_text_from_sse(&body);

        // Native tool calls (parsed from content_block_start tool_use blocks)
        // are the primary path. Only fall back to XML `<tool_call>` marker
        // parsing when no native calls were produced.
        let has_tools = request.tools.is_some();
        let (clean_text, tool_calls) = if has_tools {
            if !native_tool_calls.is_empty() {
                (text.clone(), Some(native_tool_calls))
            } else {
                // MTP fallback: parse [MIRAGE_TOOL_CALL_V1] blocks.
                let defs: Vec<crate::models::Tool> = request.tools.clone().unwrap_or_default();
                let mut st = mtp::MtpStreamState::new();
                let stripped = st.process_delta(&text, &defs);
                st.finish(&defs);
                let calls = std::mem::take(&mut st.collected_tool_calls);
                if calls.is_empty() { (text.clone(), None) } else { (stripped, Some(calls)) }
            }
        } else {
            (text.clone(), None)
        };
        let text_to_use = if tool_calls.is_some() { clean_text } else { text };

        let conv = StoredConversation {
            org_id: self.auth.org_id.clone(),
            model_id: self.model_id.clone(),
        };

        let session_url = Some(self.store_conversation(&conv, conv_token.as_deref()).await);

        let (usage_prompt, usage_completion) = extract_usage_from_sse(&body).unwrap_or_else(|| {
            let p = (prompt.len() / 4) as i32;
            (p, p)
        });

        let finish_reason = if tool_calls.is_some() { "tool_calls" } else { "stop" };

        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: self.model_id.clone(),
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: crate::models::ChatContent::String(text_to_use),
                    name: None,
                    reasoning_content: reasoning,
                    citations: if citations.is_empty() { None } else { Some(citations) },
                    tool_calls: tool_calls.clone(),
                    tool_call_id: None,
                },
                finish_reason: finish_reason.to_string(),
            }],
            usage: Usage {
                prompt_tokens: usage_prompt,
                completion_tokens: usage_completion,
                total_tokens: usage_prompt + usage_completion,
            },
            session_url,
        })
    }

    pub async fn chat_stream(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let (conv_token, stored_conv) =
            match self.resolve_conversation(&request.session_url).await? {
                Some((token, stored)) => {
                    self.session_token = Some(token.clone());
                    (Some(token), Some(stored))
                }
                None => (None, None),
            };

        let captured_org_id = stored_conv.as_ref().map(|c| c.org_id.as_str()).unwrap_or(&self.auth.org_id).to_string();

        let last_msg = request.messages.last();
        let prompt = last_msg.map(|m| m.content.as_text()).unwrap_or_default();

        let tool_messages = self
            .handle_tool_results(&request, conv_token.as_deref())
            .await;

        let mut messages = request.messages.clone();
        if !tool_messages.is_empty() {
            messages.extend(tool_messages);
        }

        let search = request.search.unwrap_or(false);
        let (file_refs, image_refs) = self.process_message_attachments(&request).await?;
        let model_id = self.model_id.clone();
        let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());

        let had_tools = request.tools.is_some();

        let http_client = self.http.clone();
        let store = self.store.clone();
        let api_base = self.api_base_url(&captured_org_id);
        let captured_org_id_for_spawn = captured_org_id.clone();
        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();
        let counter = AtomicU32::new(0);

        let fr = file_refs.clone();
        let ir = image_refs.clone();

        tokio::spawn(async move {
            // No per-session lock held - allow concurrent requests.

            let tool_prompt = match request.tools.as_ref() {
                Some(tools) => format!(
                "{}\n\nUser request:\n{}",
                mtp::build_mtp_system_prompt(tools, request.tool_choice.as_ref(), false),
                prompt
            ),
                None => String::new(),
            };

            let mut msgs = messages.clone();
            if !tool_prompt.is_empty() {
                if let Some(last) = msgs.last_mut() {
                    if last.role == "user" {
                        let original = last.content.as_text();
                        last.content = crate::models::ChatContent::String(
                            format!("{}{}", tool_prompt, original),
                        );
                    }
                }
            }

            let timezone = "Etc/UTC";
            let payload = build_native_request_payload(&request, timezone, search, &fr, &ir);
            let url = format!("{}/chat_conversations", api_base);

            let req_builder = http_client.post(&url).json(&payload);

            let resp = match send_with_retry(req_builder).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(GatewayError::Provider(format!(
                        "Claude stream request failed: {e}"
                    ))));
                    return;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let _ = tx.send(Err(GatewayError::Provider(format!(
                    "Claude API returned {status}: {}",
                    body.chars().take(200).collect::<String>()
                ))));
                return;
            }

            let mut stream = resp.bytes_stream();
            let mut buffer = Vec::new();
            let mut previous_text = String::new();
            let mut emitted_role = false;
            let mut collected_tool_calls: Vec<ToolCall> = Vec::new();
            let mut session_url: Option<String> = None;
            let mut conv_token = conv_token.clone();
            // MTP state absorbs tool blocks from content deltas inline.
            let tool_defs: Vec<crate::models::Tool> =
                request.tools.clone().unwrap_or_default();
            let mut mtp_state = mtp::MtpStreamState::new();

            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(GatewayError::Provider(format!(
                            "Claude streaming read error: {e}"
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

                        match parse_sse_line(line.trim()) {
                            Some((delta, reasoning, citations, is_done, tool_calls)) => {
                                if session_url.is_none() {
                                    let token = conv_token
                                        .clone()
                                        .unwrap_or_else(new_session_token);
                                    let stored = StoredConversation {
                                        org_id: captured_org_id_for_spawn.clone(),
                                        model_id: model_id.clone(),
                                    };
                                    store.insert(token.clone(), &stored, &model_id).await;
                                    conv_token = Some(token.clone());
                                    session_url = Some(make_session_url(&token));
                                }

                                let role = if !emitted_role && delta.is_some() {
                                    emitted_role = true;
                                    Some("assistant".to_string())
                                } else {
                                    None
                                };

                                let raw_delta = delta.unwrap_or_default();
                                let visible_delta = if tool_defs.is_empty() {
                                    raw_delta
                                } else {
                                    let visible = mtp_state.process_delta(&raw_delta, &tool_defs);
                                    for call in mtp_state.collected_tool_calls.drain(..) {
                                        if !collected_tool_calls.iter().any(|c| c.id == call.id) {
                                            collected_tool_calls.push(call);
                                        }
                                    }
                                    visible
                                };
                                build_streaming_chunk(
                                    &tx,
                                    &counter,
                                    &id_prefix,
                                    &model_id,
                                    visible_delta,
                                    reasoning,
                                    tool_calls,
                                    citations,
                                    role,
                                    &mut previous_text,
                                    &mut collected_tool_calls,
                                    session_url.as_deref(),
                                );

                                if is_done {
                                    break;
                                }
                            }
                            None => {}
                        }
                    }
                }

                buffer.drain(0..consumed);
            }

            // Post-stream tool-call extraction. Flush any pending MTP block
            // and emit collected calls so the client sees `tool_calls`.
            let mut finish_reason = "stop".to_string();
            if had_tools {
                mtp_state.finish(&tool_defs);
                let calls = std::mem::take(&mut mtp_state.collected_tool_calls);
                if !calls.is_empty() {
                    {
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
                    }
                }
            }

            if !collected_tool_calls.is_empty() {
                if let Some(ref token) = conv_token {
                    store
                        .store_tool_calls(token, &collected_tool_calls)
                        .await;
                }
            }

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

    let chunk = ChatCompletionChunk {
        id: format!("{}-{}", id_prefix, counter.load(Ordering::Relaxed)),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model_id.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta {
                role,
                content: if !delta.is_empty() { Some(delta.clone()) } else { None },
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

    #[test]
    fn extract_tool_calls_from_text_with_markers() {
        let text = r#"First I'll look up the weather.
<tool_call>{"name":"get_weather","arguments":{"city":"Paris"}}</tool_call>
And also check time.
<tool_call>{"name":"get_time","arguments":{"tz":"UTC"}}</tool_call>"#;
        let (cleaned, calls) = crate::providers::tool_call::convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[1].function.name, "get_time");
        assert!(!cleaned.contains("<tool_call>"));
        assert!(cleaned.contains("First I'll look up the weather."));
        assert!(cleaned.contains("And also check time."));
    }

    #[test]
    fn extract_tool_calls_from_text_without_markers() {
        let text = "Hello, how can I help you?";
        let (cleaned, calls) = crate::providers::tool_call::convert_xml_tool_calls(text, true);
        assert!(calls.is_none());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn extract_tool_calls_from_text_empty() {
        let text = "";
        let (cleaned, calls) = crate::providers::tool_call::convert_xml_tool_calls(text, true);
        assert!(calls.is_none());
        assert_eq!(cleaned, "");
    }

    #[test]
    fn extract_tool_calls_from_text_single_call() {
        let text = r#"<tool_call>{"name":"search","arguments":{"q":"weather"}}</tool_call>"#;
        let (cleaned, calls) = crate::providers::tool_call::convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "search");
        assert!(cleaned.is_empty());
    }

    #[test]
    fn extract_tool_calls_from_text_with_id() {
        let text = r#"<tool_call>{"name":"read_file","arguments":{"path":"/etc/hosts"},"id":"call_abc"}</tool_call>"#;
        let (_, calls) = crate::providers::tool_call::convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "read_file");
    }

    #[test]
    fn extract_tool_calls_from_text_multiline_json() {
        let text = r#"<tool_call>{
            "name":"complex",
            "arguments":{"nested":"value"}
        }</tool_call>"#;
        let (_, calls) = crate::providers::tool_call::convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "complex");
    }
}
