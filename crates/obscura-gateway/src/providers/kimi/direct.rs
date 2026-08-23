use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use sha2::Digest;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use rand::Rng;
use tracing::{info, warn};

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatMessageDelta, ChunkChoice, Citation, ToolCall, Usage,
};
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::tool_call::{
    convert_xml_tool_calls, format_tool_results, inject_tool_prompt,
};
use crate::providers::send_with_retry;
use crate::session::{SessionHandle, SessionManager};

use super::auth::{
    build_request_headers, extract_from_import, extract_refresh_token, find_refresh_token_in_import,
    navigate_to_kimi, refresh_access_token_via_api, AuthData, KIMI_API_URL,
};
use super::connectrpc;
use super::models::{resolve_model, KimiModelDef};
use super::rpc::{
    build_request_payload, collect_text_from_new_sse, collect_text_from_sse,
    collect_tool_calls_from_sse, extract_new_sse_tool_calls, parse_new_sse_line,
    parse_sse_line,
};
use super::state::{SessionStore, StoredConversation};
use super::upload::{decode_data_uri, derive_filename, upload_files};
use crate::providers::streaming_upload::download_and_hash_batch;

const TIMEOUT_SECS: u64 = 120;
const MAVIS_API_BASE: &str = "/mavis/api";

fn new_session_token() -> String {
    format!("kimi_{}", uuid::Uuid::new_v4().simple())
}

fn make_session_url(token: &str, chat_id: &str) -> String {
    format!("kimi://session/{}?chat_id={}", token, chat_id)
}

fn extract_session_token(url: &str) -> Option<String> {
    url.strip_prefix("kimi://session/")
        .and_then(|s| s.split('?').next())
        .map(|s| s.to_string())
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// ConnectRPC transport for NEW chats (kimi.ai web protocol). The JWT minted
/// on kimi.ai carries `aud=["kimi.ai"]`, which is the audience this endpoint
/// requires; legacy moonshot tokens will not work here.
///
/// Continuation turns stay on the legacy path until the ConnectRPC
/// continuation wire format is captured. Any ConnectRPC failure falls back to
/// the working legacy path, so defaulting this on is safe.
fn connect_rpc_enabled() -> bool {
    std::env::var("OBSCURA_KIMI_CONNECT_RPC")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Decode a JWT payload's `sub` claim (used as `x-traffic-id`).
fn jwt_sub(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&decoded)
        .ok()?
        .get("sub")
        .and_then(|s| s.as_str())
        .map(String::from)
}

pub struct KimiDirectClient {
    http: Client,
    #[allow(dead_code)]
    auth: AuthData,
    store: SessionStore,
    model_id: String,
    model_def: KimiModelDef,
    session_token: Option<String>,
}

impl KimiDirectClient {
    pub async fn new(
        session: &SessionHandle,
        model_id: &str,
        sessions: &SessionManager,
        store: SessionStore,
    ) -> Result<Self, GatewayError> {
        let model_def = resolve_model(model_id).ok_or_else(|| {
            GatewayError::BadRequest(format!("unknown Kimi model: {model_id}"))
        })?;

        // A Firefox profile is a snapshot. Always give the live page a chance
        // to refresh a server-invalidated token before trusting imported data,
        // even when the JWT's local exp claim is still in the future.
        // Server-side refresh from the imported refresh token BEFORE any
        // navigation: if the copied access token is expired, loading kimi.ai
        // in the automation profile shows the login panel (which wipes
        // localStorage), so the page-JS path cannot recover. The refresh
        // token itself is long-lived and works without a page.
        let imported = extract_from_import(
            &session.local_storage,
            Some(session.cookie_jar.as_ref()),
        );
        let has_valid_import = imported.is_some();
        let refreshed = if has_valid_import {
            None // valid access token already available
        } else {
            match find_refresh_token_in_import(&session.local_storage) {
                Some(rt) => {
                    let device_id = format!(
                        "{:016}",
                        rand::thread_rng().gen_range(0..10_000_000_000_000_000_u64)
                    );
                    match refresh_access_token_via_api(&rt, &device_id).await {
                        Ok(data) => Some(data),
                        Err(e) => {
                            warn!(error = %e, "Kimi server-side token refresh failed; trying page path");
                            None
                        }
                    }
                }
                None => None,
            }
        };

        let auth = match navigate_to_kimi(sessions, &session.id).await {
            Ok(()) => match extract_refresh_token(
                sessions,
                &session.id,
                Some(session.cookie_jar.as_ref()),
            )
            .await
            {
                Ok(data) => {
                    info!("Kimi auth extracted after live page refresh");
                    data
                }
                Err(page_error) => refreshed.or(imported).ok_or(page_error)?,
            },
            Err(error) => refreshed.or(imported).ok_or(error)?,
        };

        let headers = build_request_headers(&auth.access_token, &auth.device_id)?;

        let http = Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .default_headers(headers)
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;

        info!(
            session_id = %session.id,
            model = %model_id,
            "Kimi DirectClient initialized"
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
    /// Kimi's web client (chat.kimi.com) models a chat as a single linear
    /// message chain; there is no message-tree or fork-from-a-prior-message
    /// flow in the web API surface. Continuations serialize on one shared
    /// tip, matching the web app. No lock is held; the shared store is the
    /// single source of truth and the reused token is the continuity handle.
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
        make_session_url(&token, &conv.chat_id)
    }

    async fn create_chat(&self) -> Result<String, GatewayError> {
        let payload = serde_json::json!({
            "name": "New Chat",
            "born_from": "home",
            "kimiplus_id": self.model_def.kimiplus_id,
            "is_example": false,
            "source": "web",
            "tags": [],
        });

        let resp = self
            .http
            .post(format!("{}/api/chat", KIMI_API_URL))
            .json(&payload)
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("create chat failed: {e}")))?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| GatewayError::Provider(format!("create chat parse failed: {e}")))?;

        if !status.is_success() {
            return Err(GatewayError::Provider(format!(
                "create chat returned {status}: {body}"
            )));
        }

        body["id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| GatewayError::Provider("no id in create chat response".to_string()))
    }

    async fn send_stream_request(
        &self,
        messages: &[ChatMessage],
        conv: Option<&StoredConversation>,
        chat_id: &str,
        tool_prompt: &str,
        search: bool,
        request: &ChatCompletionRequest,
        refs: &[String],
        refs_file: &[String],
    ) -> Result<reqwest::Response, GatewayError> {
        let mut msgs = messages.to_vec();

        if !tool_prompt.is_empty() {
            if let Some(last) = msgs.last_mut() {
                if last.role == "user" {
                    let original = last.content.as_text();
                    last.content = crate::models::ChatContent::String(format!("{}{}", tool_prompt, original));
                }
            }
        }

        let payload = build_request_payload(&msgs, conv, &self.model_def, search, request, refs, refs_file);

        let url = format!("{}/api/chat/{}/completion/stream", KIMI_API_URL, chat_id);

        let mut req_builder = self.http.post(&url).json(&payload);

        let thinking_enabled = request.thinking.unwrap_or(self.model_def.is_thinking);
        if thinking_enabled {
            req_builder = req_builder.header("x-msh-thinking", "1");
        }

        let resp = send_with_retry(req_builder)
            .await
            .map_err(|e| GatewayError::Provider(format!("stream request failed: {e}")))?;

        Ok(resp)
    }

    /// Build the payload for the new Kimi message API (K3+).
    fn build_session_message_payload(
        content: &str,
        model_id: &str,
        turn_id: Option<&str>,
        image_refs: &[String],
        file_refs: &[String],
        use_thinking: bool,
    ) -> serde_json::Value {
        let model = serde_json::json!({
            "provider_id": "kimi",
            "model_id": model_id,
        });

        let mut attachments: Vec<serde_json::Value> = Vec::new();
        for r in image_refs {
            attachments.push(serde_json::json!({
                "type": "image",
                "file_path": r,
            }));
        }
        for r in file_refs {
            attachments.push(serde_json::json!({
                "type": "file",
                "file_path": r,
            }));
        }

        let mut payload = serde_json::json!({
            "content": content,
            "model": model,
        });

        if !attachments.is_empty() {
            payload["attachments"] = serde_json::Value::Array(attachments);
        }

        if let Some(tid) = turn_id {
            payload["turn_id"] = serde_json::json!(tid);
        }

        // The new API uses x-msh-thinking header instead of a payload field,
        // but we set a client_intent field for reasoning effort
        if use_thinking {
            payload["client_intent"] = serde_json::json!("thinking");
        }

        payload
    }

    /// Send a message to an existing chat session using the new Kimi message API
    /// (used by K3 and future models).
    ///
    /// Endpoint: `POST /mavis/api/session/{chat_id}/message`
    ///
    /// Unlike the legacy `completion/stream` endpoint, this receives SSE events
    /// with a JSON `type` discriminator field.
    async fn send_connect_chat(&self, body: &serde_json::Value) -> Result<reqwest::Response, GatewayError> {
        let traffic_id = jwt_sub(&self.auth.access_token).unwrap_or_default();
        let headers = connectrpc::build_headers(
            &self.auth.access_token,
            &self.auth.device_id,
            &self.auth.device_id,
            &traffic_id,
        );
        let frame = connectrpc::encode_frame(body.to_string().as_bytes());
        let mut req = self.http.post(connectrpc::CONNECT_CHAT_URL);
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req.body(frame)
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("Kimi ConnectRPC request failed: {e}")))
    }

    /// Stream one turn over the kimi.ai ConnectRPC transport (new chats only;
    /// continuation stays on the legacy path until the continuation wire
    /// format is captured).
    async fn chat_stream_connect(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let last_msg = request.messages.last();
        let content = last_msg.map(|m| m.content.as_text()).unwrap_or_default();

        // MTP system prompt when the client supplied tools.
        let final_content = match request.tools.as_ref() {
            Some(tools) if !tools.is_empty() => format!(
                "{}\n\nUser request:\n{}",
                crate::providers::mtp::build_mtp_system_prompt(
                    tools,
                    request.tool_choice.as_ref(),
                    false
                ),
                content
            ),
            _ => content,
        };

        let thinking = request.thinking.unwrap_or(self.model_def.is_thinking);
        let search = request.search.unwrap_or(false);
        let body = connectrpc::build_chat_body(&final_content, thinking, search, None);

        let resp = self.send_connect_chat(&body).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(GatewayError::Provider(format!(
                "Kimi ConnectRPC returned {status}"
            )));
        }
        let byte_stream = resp.bytes_stream();

        let model_id = self.model_id.clone();
        let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let tool_defs = request.tools.clone().unwrap_or_default();
        let store = self.store.clone();

        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut decoder = connectrpc::FrameDecoder::new();
            let mut mtp_state = crate::providers::mtp::MtpStreamState::new();
            let mut saw_tool_calls = false;
            let mut chat_id: Option<String> = None;
            let mut assistant_message_id: Option<String> = None;
            let mut role_emitted = false;
            let mut stream = byte_stream;

            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(GatewayError::Provider(format!(
                            "Kimi ConnectRPC stream error: {e}"
                        ))));
                        return;
                    }
                };
                decoder.push(&bytes);
                for event in decoder.drain() {
                    match connectrpc::classify_event(&event) {
                        connectrpc::KimiEvent::ThinkDelta(t) => {
                            if t.is_empty() {
                                continue;
                            }
                            let _ = tx.send(Ok(ChatCompletionChunk {
                                id: id_prefix.clone(),
                                object: "chat.completion.chunk".to_string(),
                                created: current_timestamp(),
                                model: model_id.clone(),
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
                        connectrpc::KimiEvent::TextDelta(t) => {
                            if t.is_empty() {
                                continue;
                            }
                            let visible = if tool_defs.is_empty() {
                                t
                            } else {
                                let clean = mtp_state.process_delta(&t, &tool_defs);
                                if !mtp_state.collected_tool_calls.is_empty() {
                                    let calls =
                                        std::mem::take(&mut mtp_state.collected_tool_calls);
                                    saw_tool_calls = true;
                                    let _ = tx.send(Ok(ChatCompletionChunk {
                                        id: id_prefix.clone(),
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
                                            finish_reason: None,
                                        }],
                                        session_url: None,
                                    }));
                                }
                                clean
                            };
                            if visible.is_empty() && !saw_tool_calls {
                                continue;
                            }
                            let role = if !role_emitted && !visible.is_empty() {
                                role_emitted = true;
                                Some("assistant".to_string())
                            } else {
                                None
                            };
                            let _ = tx.send(Ok(ChatCompletionChunk {
                                id: id_prefix.clone(),
                                object: "chat.completion.chunk".to_string(),
                                created: current_timestamp(),
                                model: model_id.clone(),
                                choices: vec![ChunkChoice {
                                    index: 0,
                                    delta: ChatMessageDelta {
                                        role,
                                        content: if visible.is_empty() { None } else { Some(visible) },
                                        reasoning_content: None,
                                        citations: None,
                                        tool_calls: None,
                                    },
                                    finish_reason: None,
                                }],
                                session_url: None,
                            }));
                        }
                        connectrpc::KimiEvent::ChatId(id) => chat_id = Some(id),
                        connectrpc::KimiEvent::MessageId(id) => assistant_message_id = Some(id),
                        connectrpc::KimiEvent::Done | connectrpc::KimiEvent::Heartbeat => {}
                        connectrpc::KimiEvent::References(_) => {}
                        connectrpc::KimiEvent::Other => {}
                    }
                }
            }

            // Flush any pending MTP block at stream end.
            if !tool_defs.is_empty() {
                mtp_state.finish(&tool_defs);
                let calls = std::mem::take(&mut mtp_state.collected_tool_calls);
                if !calls.is_empty() {
                    saw_tool_calls = true;
                    let _ = tx.send(Ok(ChatCompletionChunk {
                        id: id_prefix.clone(),
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
                            finish_reason: None,
                        }],
                        session_url: None,
                    }));
                }
            }

            // Persist conversation ids so the next turn can continue.
            if let Some(chat_id) = chat_id {
                let token = new_session_token();
                store
                    .insert(
                        token.clone(),
                        &StoredConversation {
                            chat_id: chat_id.clone(),
                            model_id: model_id.clone(),
                            segment_id: assistant_message_id,
                        },
                        &model_id,
                    )
                    .await;
                let session_url = make_session_url(&token, &chat_id);
                let finish_reason = if saw_tool_calls { "tool_calls" } else { "stop" };
                let _ = tx.send(Ok(ChatCompletionChunk {
                    id: format!("{id_prefix}-final"),
                    object: "chat.completion.chunk".to_string(),
                    created: current_timestamp(),
                    model: model_id.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChatMessageDelta::default(),
                        finish_reason: Some(finish_reason.to_string()),
                    }],
                    session_url: Some(session_url),
                }));
            } else {
                // No chat id seen: upstream did not accept the turn.
                let _ = tx.send(Err(GatewayError::Provider(
                    "Kimi ConnectRPC stream ended without a chat id".to_string(),
                )));
            }
        });

        Ok(UnboundedReceiverStream::new(rx).boxed())
    }

    /// Send a message to an existing chat session using the new Kimi message API
    /// (used by K3 and future models).
    ///
    /// Endpoint: `POST /mavis/api/session/{chat_id}/message`
    ///
    /// Unlike the legacy `completion/stream` endpoint, this receives SSE events
    /// with a JSON `type` discriminator field.
    async fn send_session_message(
        &self,
        chat_id: &str,
        content: &str,
        turn_id: Option<&str>,
        image_refs: &[String],
        file_refs: &[String],
        use_thinking: bool,
    ) -> Result<reqwest::Response, GatewayError> {
        let payload = Self::build_session_message_payload(
            content,
            &self.model_id,
            turn_id,
            image_refs,
            file_refs,
            use_thinking,
        );

        let url = format!(
            "{}{}/session/{}/message",
            KIMI_API_URL, MAVIS_API_BASE, chat_id,
        );

        let mut req_builder = self.http.post(&url).json(&payload);

        if use_thinking {
            req_builder = req_builder.header("x-msh-thinking", "1");
        }

        let resp = send_with_retry(req_builder)
            .await
            .map_err(|e| GatewayError::Provider(format!("session message request failed: {e}")))?;

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

    /// Process image and file attachments from the last user message.
    /// Returns uploaded file reference IDs.
    async fn process_message_attachments(
        &self,
        request: &ChatCompletionRequest,
        chat_id: &str,
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

        // Process all URLs concurrently
        async fn process_urls(
            http: &Client,
            urls: &[String],
            chat_id: &str,
            cache: &super::upload::UploadCache,
            is_image: bool,
        ) -> Result<Vec<String>, GatewayError> {
            let mut pending: Vec<(Vec<u8>, String)> = Vec::new();
            let mut resolved: Vec<String> = Vec::new();

            // Process data URIs
            for url in urls {
                if let Some((data, mime)) = decode_data_uri(url) {
                    let hash = base64::engine::general_purpose::STANDARD
                        .encode(sha2::Sha256::digest(&data));
                    let name = derive_filename(url, &mime);
                    if let Some(cached) = cache.get(&hash).await {
                        resolved.push(cached);
                    } else {
                        pending.push((data, name));
                    }
                }
            }

            // Concurrently download remote URLs
            let remote_urls: Vec<String> = urls
                .iter()
                .filter(|u| !u.starts_with("data:"))
                .cloned()
                .collect();

            if !remote_urls.is_empty() {
                match download_and_hash_batch(http, remote_urls).await {
                    Ok(hashed_files) => {
                        for hashed in hashed_files {
                            if let Some(cached) = cache.get(&hashed.hash_base64).await {
                                resolved.push(cached);
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
                match upload_files(http, &pending, chat_id, cache, is_image).await {
                    Ok(ids) => resolved.extend(ids),
                    Err(e) => tracing::warn!(error = %e, "attachment upload failed"),
                }
            }

            Ok(resolved)
        }

        let image_refs = process_urls(
            &self.http, &image_urls, chat_id, cache, true,
        )
        .await?;
        let file_refs = process_urls(
            &self.http, &file_urls, chat_id, cache, false,
        )
        .await?;

        Ok((image_refs, file_refs))
    }

    /// Returns true if this model uses the new `/mavis/api/session/{id}/message` API
    /// instead of the legacy `/api/chat/{id}/completion/stream` endpoint.
    fn uses_message_api(&self) -> bool {
        matches!(self.model_id.as_str(), "kimi-k3" | "kimi-k3-instant" | "kimi-k3-swarm")
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

        let last_msg = request.messages.last();
        let prompt = last_msg.map(|m| m.content.as_text()).unwrap_or_default();

        let tool_messages = self
            .handle_tool_results(&request, conv_token.as_deref())
            .await;

        let mut messages = request.messages.clone();
        if !tool_messages.is_empty() {
            messages.extend(tool_messages);
        }

        let chat_id = match stored_conv.as_ref() {
            Some(c) => c.chat_id.clone(),
            None => self.create_chat().await?,
        };

        let search = request.search.unwrap_or(false);
        let (image_refs, file_refs) = self.process_message_attachments(&request, &chat_id).await?;
        let thinking_enabled = request.thinking.unwrap_or(self.model_def.is_thinking);

        if self.uses_message_api() {
            // New API path (K3+): send message via /mavis/api/session/{id}/message
            let user_content = messages
                .iter()
                .map(|m| m.content.as_text())
                .collect::<Vec<_>>()
                .join("\n");

            let turn_id = stored_conv
                .as_ref()
                .and_then(|c| c.segment_id.as_deref());

            let tool_prompt = match request.tools.as_ref() {
                Some(tools) => inject_tool_prompt("kimi", &user_content, tools, request.tool_choice.as_ref()),
                None => String::new(),
            };
            let final_content = if tool_prompt.is_empty() {
                user_content
            } else {
                format!("{}{}", tool_prompt, user_content)
            };

            let resp = self
                .send_session_message(&chat_id, &final_content, turn_id, &image_refs, &file_refs, thinking_enabled)
                .await?;

            let status = resp.status();
            let mut stream = resp.bytes_stream();
            let mut body = Vec::new();
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.map_err(|e| {
                    GatewayError::Provider(format!("response read failed: {e}"))
                })?;
                body.extend_from_slice(&bytes);
            }

            if !status.is_success() {
                return Err(GatewayError::Provider(format!(
                    "Kimi session message API returned {status}: {}",
                    String::from_utf8_lossy(&body).chars().take(200).collect::<String>()
                )));
            }

            let body_str = String::from_utf8_lossy(&body);
            tracing::debug!(raw_response = %body_str, "K3 raw response body");
            let (text, thinking, turn_id) = collect_text_from_new_sse(&body);

            // Empty 200 with a non-empty body is the protocol-drift
            // signature (the "1 chunk, 0 deltas" failure class). Snapshot
            // the raw body for healing before it is dropped.
            if text.is_empty() && thinking.as_deref().is_none() && !body.is_empty() {
                crate::providers::drift_snapshot::global()
                    .record("kimi", "empty-200", &body);
            }

            // Extract tool calls from the response if tools were requested
            let has_tools = request.tools.is_some();
            let native_tool_calls = if has_tools {
                let native = body
                    .split(|&b| b == b'\n')
                    .filter_map(|line| std::str::from_utf8(line).ok())
                    .find_map(extract_new_sse_tool_calls);
                if native.is_some() {
                    tracing::info!("K3 message API returned native tool_calls");
                }
                native
            } else {
                None
            };
            let (clean_text, tool_calls) = if let Some(native) = native_tool_calls {
                (text.clone(), Some(native))
            } else if has_tools {
                tracing::info!("K3 path: falling back to XML tool-call parsing");
                convert_xml_tool_calls(&text, true)
            } else {
                (text.clone(), None)
            };
            let text_to_use = if tool_calls.is_some() { clean_text } else { text };
            let finish_reason = if tool_calls.is_some() { "tool_calls".to_string() } else { "stop".to_string() };

            let conv = StoredConversation {
                chat_id: chat_id.clone(),
                model_id: self.model_id.clone(),
                segment_id: turn_id,
            };

            let session_url = Some(self.store_conversation(&conv, conv_token.as_deref()).await);

            // Store tool calls for multi-turn continuation
            if let Some(ref calls) = tool_calls {
                let token_to_use = session_url
                    .as_deref()
                    .and_then(extract_session_token)
                    .or_else(|| conv_token.clone());
                if let Some(token) = token_to_use.as_deref() {
                    self.store.store_tool_calls(token, calls).await;
                }
            }

            let prompt_text: String = request.messages.iter().map(|m| m.content.as_text()).collect();
            let completion_text = text_to_use.clone();

            return Ok(ChatCompletionResponse {
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
                        reasoning_content: thinking,
                        citations: None,
                        tool_calls: tool_calls.clone(),
                        tool_call_id: None,
                    },
                    finish_reason,
                }],
                usage: Usage {
                    prompt_tokens: estimate_tokens("kimi", &self.model_id, &prompt_text),
                    completion_tokens: estimate_tokens("kimi", &self.model_id, &completion_text),
                    total_tokens: estimate_tokens("kimi", &self.model_id, &prompt_text)
                        + estimate_tokens("kimi", &self.model_id, &completion_text),
                },
                session_url,
            });
        }

        let tool_prompt = match request.tools.as_ref() {
            Some(tools) => inject_tool_prompt("kimi", &prompt, tools, request.tool_choice.as_ref()),
            None => String::new(),
        };

        let resp = self
            .send_stream_request(&messages, stored_conv.as_ref(), &chat_id, &tool_prompt, search, &request, &file_refs, &image_refs)
            .await?;

        let status = resp.status();

        // Read the streaming body chunk by chunk (same as streaming path)
        // to avoid issues with resp.bytes() on chunked transfer encoding.
        let mut stream = resp.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| {
                GatewayError::Provider(format!("response read failed: {e}"))
            })?;
            body.extend_from_slice(&bytes);
        }

        if !status.is_success() {
            return Err(GatewayError::Provider(format!(
                "Kimi API returned {status}: {}",
                String::from_utf8_lossy(&body).chars().take(200).collect::<String>()
            )));
        }

        let (text, thinking) = collect_text_from_sse(&body);

        // Try native tool_calls from API first, fall back to XML marker parsing
        let has_tools = request.tools.is_some();
        let native_tool_calls = if has_tools {
            let native = collect_tool_calls_from_sse(&body);
            if native.is_some() {
                tracing::info!("Kimi API returned native tool_calls in SSE event");
            } else {
                tracing::info!(raw_text = %text.chars().take(300).collect::<String>(), "Kimi API did NOT return native tool_calls, will parse XML from text");
            }
            native
        } else {
            None
        };
        let (clean_text, tool_calls) = if let Some(native) = native_tool_calls {
            tracing::info!("Using native tool_calls from API");
            (text.clone(), Some(native))
        } else if has_tools {
            tracing::info!("Falling back to XML marker parsing from text");
            convert_xml_tool_calls(&text, true)
        } else {
            (text.clone(), None)
        };
        let text_to_use = if tool_calls.is_some() { clean_text } else { text };

        let body_str = std::str::from_utf8(&body).unwrap_or("");
        let segment_id: Option<String> = body_str.lines().find_map(|line| {
            if let Some((_, _, seg_id, _, _, _, _, _)) = parse_sse_line(line) {
                seg_id
            } else {
                None
            }
        });

        let conv = StoredConversation {
            chat_id: chat_id.clone(),
            model_id: self.model_id.clone(),
            segment_id,
        };

        let session_url = Some(self.store_conversation(&conv, conv_token.as_deref()).await);

        let prompt_text: String = request.messages.iter().map(|m| m.content.as_text()).collect();
        let completion_text = text_to_use.clone();

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
                    reasoning_content: thinking,
                    citations: None,
                    tool_calls: tool_calls.clone(),
                    tool_call_id: None,
                },
                finish_reason: finish_reason.to_string(),
            }],
            usage: Usage {
                prompt_tokens: estimate_tokens("kimi", &self.model_id, &prompt_text),
                completion_tokens: estimate_tokens("kimi", &self.model_id, &completion_text),
                total_tokens: estimate_tokens("kimi", &self.model_id, &prompt_text)
                    + estimate_tokens("kimi", &self.model_id, &completion_text),
            },
            session_url,
        })
    }

    pub async fn chat_stream(
        &mut self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        // ConnectRPC transport for NEW chats when opted in; any failure
        // falls through to the legacy path below.
        if connect_rpc_enabled() && request.session_url.is_none() {
            match self.chat_stream_connect(request.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    tracing::warn!(error = %e, "Kimi ConnectRPC path failed; falling back to legacy SSE");
                }
            }
        }

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

        let tool_messages = self
            .handle_tool_results(&request, conv_token.as_deref())
            .await;

        let mut messages = request.messages.clone();
        if !tool_messages.is_empty() {
            messages.extend(tool_messages);
        }

        let chat_id = match stored_conv.as_ref() {
            Some(c) => c.chat_id.clone(),
            None => self.create_chat().await?,
        };

        let model_id = self.model_id.clone();
        let model_def = self.model_def.clone();
        let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());

        let search = request.search.unwrap_or(false);

        let (image_refs, file_refs) = self.process_message_attachments(&request, &chat_id).await?;

        let had_tools = request.tools.is_some();
        let thinking_enabled = request.thinking.unwrap_or(model_def.is_thinking);

        // K3+ models use the new session message API
        if self.uses_message_api() {
            let store = self.store.clone();
            let http_client = self.http.clone();
            let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();
            let stored_chat_id = chat_id.clone();
            let stored_model_id = model_id.clone();
            // Allocate the gateway continuation handle before reading the
            // upstream stream. K3 may omit `turn_id` on a successful stream,
            // but the chat id is still sufficient for the next message.
            let stream_token = conv_token
                .clone()
                .unwrap_or_else(new_session_token);
            let initial_session_url = make_session_url(&stream_token, &chat_id);
            let initial_conv = StoredConversation {
                chat_id: chat_id.clone(),
                model_id: model_id.clone(),
                segment_id: stored_conv.as_ref().and_then(|c| c.segment_id.clone()),
            };
            store.insert(stream_token.clone(), &initial_conv, &model_id).await;
            let stored_conv_token = Some(stream_token);
            let id_prefix2 = id_prefix.clone();
            let file_refs_k3 = file_refs.clone();
            let image_refs_k3 = image_refs.clone();

            tokio::spawn(async move {
                let user_content = messages
                    .iter()
                    .map(|m| m.content.as_text())
                    .collect::<Vec<_>>()
                    .join("\n");

                let tool_prompt = match request.tools.as_ref() {
                    Some(tools) => inject_tool_prompt("kimi", &prompt, tools, request.tool_choice.as_ref()),
                    None => String::new(),
                };
                let final_content = if tool_prompt.is_empty() {
                    user_content
                } else {
                    format!("{}{}", tool_prompt, user_content)
                };

                let turn_id = stored_conv
                    .as_ref()
                    .and_then(|c| c.segment_id.as_deref());

                let payload = KimiDirectClient::build_session_message_payload(
                    &final_content,
                    &stored_model_id,
                    turn_id,
                    &image_refs_k3,
                    &file_refs_k3,
                    thinking_enabled,
                );

                let url = format!(
                    "{}{}/session/{}/message",
                    KIMI_API_URL, MAVIS_API_BASE, stored_chat_id,
                );

                let mut req_builder = http_client.post(&url).json(&payload);
                if thinking_enabled {
                    req_builder = req_builder.header("x-msh-thinking", "1");
                }

                let resp = match send_with_retry(req_builder).await {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(GatewayError::Provider(format!(
                            "Kimi session message stream failed: {e}"
                        ))));
                        return;
                    }
                };

                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    let _ = tx.send(Err(GatewayError::Provider(format!(
                        "Kimi session message API returned {status}: {}",
                        body.chars().take(200).collect::<String>()
                    ))));
                    return;
                }

                let mut stream = resp.bytes_stream();
                let mut buffer = Vec::new();
                let mut previous_text = String::new();
                let mut emitted_role = false;
                let mut collected_tool_calls: Vec<ToolCall> = Vec::new();
                let mut session_url: Option<String> = Some(initial_session_url.clone());
                let mut stored_turn_id: Option<String> = None;
                let mut conv_token = stored_conv_token.clone();
                let counter = AtomicU32::new(0);
                let mut xml_tool_state = crate::providers::tool_call::XmlToolCallStripper::new();

                while let Some(chunk) = stream.next().await {
                    let bytes = match chunk {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = tx.send(Err(GatewayError::Provider(format!(
                                "Kimi session message read error: {e}"
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

                            let trimmed = line.trim();
                            // Log all raw SSE lines for K3 debugging
                            if !trimmed.is_empty() && !trimmed.starts_with("data: {\"type\":\"heartbeat") {
                                let preview = trimmed.chars().take(400).collect::<String>();
                                tracing::debug!("K3 SSE raw: {preview}");
                            }

                            if had_tools {
                                if let Some(native_calls) = extract_new_sse_tool_calls(trimmed) {
                                    build_streaming_chunk(
                                        &tx,
                                        &counter,
                                        &id_prefix2,
                                        &stored_model_id,
                                        String::new(),
                                        None,
                                        Some(native_calls),
                                        None,
                                        None,
                                        &mut previous_text,
                                        &mut collected_tool_calls,
                                        session_url.as_deref(),
                                    );
                                }
                            }

                            match parse_new_sse_line(trimmed) {
                                Some((delta, think, turn_id, is_done, is_error, error_msg)) => {
                                    if is_error {
                                        let msg = error_msg.unwrap_or_else(|| "Kimi error".to_string());
                                        let _ = tx.send(Err(GatewayError::Provider(msg)));
                                        return;
                                    }

                                    if let Some(ref tid) = turn_id {
                                        stored_turn_id = Some(tid.clone());
                                        let conv = StoredConversation {
                                            chat_id: stored_chat_id.clone(),
                                            model_id: stored_model_id.clone(),
                                            segment_id: Some(tid.clone()),
                                        };
                                        let token = conv_token
                                            .clone()
                                            .unwrap_or_else(new_session_token);
                                        store.insert(token.clone(), &conv, &stored_model_id).await;
                                        conv_token = Some(token.clone());
                                        session_url = Some(make_session_url(&token, &stored_chat_id));
                                    }

                                    // Strip XML tool call markers from delta during
                                    // streaming so raw <tool_call> tags never leak
                                    // into content chunks (same as the legacy path).
                                    // `complete_message` may repeat the full response after
                                    // incremental deltas. Emit only the unseen suffix so a
                                    // streaming client does not receive duplicated text.
                                    let delta = delta.map(|d| {
                                        if d.starts_with(&previous_text) {
                                            d[previous_text.len()..].to_string()
                                        } else if previous_text.starts_with(&d) {
                                            String::new()
                                        } else {
                                            d
                                        }
                                    });
                                    let (clean_delta, xml_tool_call) = if had_tools {
                                        if let Some(d) = &delta {
                                            xml_tool_state.process(d)
                                        } else {
                                            (String::new(), None)
                                        }
                                    } else {
                                        (delta.clone().unwrap_or_default(), None)
                                    };

                                    // Emit streaming XML tool call if found
                                    if let Some(tc) = xml_tool_call {
                                        if !collected_tool_calls.iter().any(|c| c.id == tc.id) {
                                            collected_tool_calls.push(tc.clone());
                                            let idx = counter.fetch_add(1, Ordering::Relaxed);
                                            let _ = tx.send(Ok(ChatCompletionChunk {
                                                id: format!("{}-{}", id_prefix2, idx),
                                                object: "chat.completion.chunk".to_string(),
                                                created: current_timestamp(),
                                                model: stored_model_id.clone(),
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

                                    let has_content_clean = !clean_delta.is_empty();
                                    let has_think = think.is_some();
                                    if has_content_clean || has_think {
                                        let role = if !emitted_role && has_content_clean {
                                            emitted_role = true;
                                            Some("assistant".to_string())
                                        } else {
                                            None
                                        };

                                        build_streaming_chunk(
                                            &tx,
                                            &counter,
                                            &id_prefix2,
                                            &stored_model_id,
                                            clean_delta,
                                            think,
                                            None,
                                            None,
                                            role,
                                            &mut previous_text,
                                            &mut collected_tool_calls,
                                            session_url.as_deref(),
                                        );
                                    }

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

                // Post-stream: add any leftover buffered XML tool call
                if had_tools {
                    if let Some(tc) = xml_tool_state.finish_pending() {
                        if !collected_tool_calls.iter().any(|c| c.id == tc.id) {
                            collected_tool_calls.push(tc.clone());
                            let idx = counter.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(Ok(ChatCompletionChunk {
                                id: format!("{}-{}", id_prefix2, idx),
                                object: "chat.completion.chunk".to_string(),
                                created: current_timestamp(),
                                model: stored_model_id.clone(),
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

                let finish_reason = if had_tools && !collected_tool_calls.is_empty() {
                    "tool_calls".to_string()
                } else {
                    "stop".to_string()
                };

                let conv = StoredConversation {
                    chat_id: stored_chat_id.clone(),
                    model_id: stored_model_id.clone(),
                    segment_id: stored_turn_id.clone(),
                };
                let token = conv_token
                    .clone()
                    .unwrap_or_else(new_session_token);
                store.insert(token.clone(), &conv, &stored_model_id).await;

                if !collected_tool_calls.is_empty() {
                    store
                        .store_tool_calls(&token, &collected_tool_calls)
                        .await;
                }
                if session_url.is_none() {
                    session_url = Some(make_session_url(&token, &stored_chat_id));
                }

                let _ = tx.send(Ok(ChatCompletionChunk {
                    id: format!("{}-final", id_prefix2),
                    object: "chat.completion.chunk".to_string(),
                    created: current_timestamp(),
                    model: stored_model_id,
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChatMessageDelta::default(),
                        finish_reason: Some(finish_reason),
                    }],
                    session_url,
                }));
            });

            return Ok(UnboundedReceiverStream::new(rx).boxed());
        }

        let http_client = self.http.clone();
        let store = self.store.clone();
        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();
        let counter = AtomicU32::new(0);
        let file_refs_stream = file_refs.clone();
        let image_refs_stream = image_refs.clone();

        tokio::spawn(async move {
            let tool_prompt = match request.tools.as_ref() {
                Some(tools) => inject_tool_prompt("kimi", &prompt, tools, request.tool_choice.as_ref()),
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

            let payload = build_request_payload(&msgs, stored_conv.as_ref(), &model_def, search, &request, &file_refs_stream, &image_refs_stream);

            let url = format!("{}/api/chat/{}/completion/stream", KIMI_API_URL, chat_id);

            let mut req_builder = http_client.post(&url).json(&payload);

            let thinking_enabled = request.thinking.unwrap_or(model_def.is_thinking);
            if thinking_enabled {
                req_builder = req_builder.header("x-msh-thinking", "1");
            }

            let resp = match send_with_retry(req_builder).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(GatewayError::Provider(format!(
                        "Kimi stream request failed: {e}"
                    ))));
                    return;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let _ = tx.send(Err(GatewayError::Provider(format!(
                    "Kimi API returned {status}: {}",
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
            let stored_chat_id = chat_id.clone();
            let mut stored_segment_id: Option<String> = None;
            let mut conv_token = conv_token.clone();
            let mut xml_tool_state = crate::providers::tool_call::XmlToolCallStripper::new();

            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(GatewayError::Provider(format!(
                            "Kimi streaming read error: {e}"
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
                            Some((delta, citations, segment_id, is_done, is_length, is_error, think_delta, tool_calls_raw)) => {
                                if tool_calls_raw.is_some() {
                                    tracing::info!(?tool_calls_raw, "Kimi streaming: got native tool_calls from SSE");
                                }

                                if is_error {
                                    let _ = tx.send(Err(GatewayError::Provider(
                                        "Kimi content policy violation".to_string(),
                                    )));
                                    return;
                                }

                                if is_length {
                                    let _ = tx.send(Err(GatewayError::Provider(
                                        "Kimi max tokens reached".to_string(),
                                    )));
                                    return;
                                }

                                if let Some(ref seg_id) = segment_id {
                                    stored_segment_id = Some(seg_id.clone());
                                    let conv = StoredConversation {
                                        chat_id: stored_chat_id.clone(),
                                        model_id: model_id.clone(),
                                        segment_id: Some(seg_id.clone()),
                                    };
                                    let token = conv_token
                                        .clone()
                                        .unwrap_or_else(new_session_token);
                                    store.insert(token.clone(), &conv, &model_id).await;
                                    conv_token = Some(token.clone());
                                    session_url = Some(make_session_url(&token, &stored_chat_id));
                                }

                                // Strip XML tool call markers from delta during streaming
                                let (clean_delta, xml_tool_call) = if had_tools {
                                    if let Some(d) = &delta {
                                        xml_tool_state.process(d)
                                    } else {
                                        (String::new(), None)
                                    }
                                } else {
                                    (delta.clone().unwrap_or_default(), None)
                                };

                                // Emit streaming XML tool call if found
                                if let Some(tc) = xml_tool_call {
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

                                let has_content_clean = !clean_delta.is_empty();
                                let has_citations = citations.is_some();
                                let has_think = think_delta.is_some();
                                if has_content_clean || has_citations || has_think {
                                    let role = if !emitted_role && has_content_clean {
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
                                        tool_calls_raw,
                                        citations,
                                        role,
                                        &mut previous_text,
                                        &mut collected_tool_calls,
                                        session_url.as_deref(),
                                    );
                                }

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

            // Post-stream: add any leftover buffered XML tool call
            if had_tools {
                if let Some(tc) = xml_tool_state.finish_pending() {
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

            let finish_reason = if had_tools && !collected_tool_calls.is_empty() {
                "tool_calls".to_string()
            } else {
                "stop".to_string()
            };

            if let Some(ref seg_id) = stored_segment_id {
                let conv = StoredConversation {
                    chat_id: stored_chat_id.clone(),
                    model_id: model_id.clone(),
                    segment_id: Some(seg_id.clone()),
                };
                let token = conv_token
                    .clone()
                    .unwrap_or_else(new_session_token);
                store.insert(token.clone(), &conv, &model_id).await;

                if !collected_tool_calls.is_empty() {
                    store
                        .store_tool_calls(&token, &collected_tool_calls)
                        .await;
                }

                if session_url.is_none() {
                    session_url = Some(make_session_url(&token, &stored_chat_id));
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

    let clean_delta = if delta.is_empty() && new_tool_calls.is_none() {
        String::new()
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
                content: if !clean_delta.is_empty() { Some(clean_delta) } else { None },
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
        let (cleaned, calls) = convert_xml_tool_calls(text, true);
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
        let (cleaned, calls) = convert_xml_tool_calls(text, true);
        assert!(calls.is_none());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn extract_tool_calls_from_text_empty() {
        let text = "";
        let (cleaned, calls) = convert_xml_tool_calls(text, true);
        assert!(calls.is_none());
        assert_eq!(cleaned, "");
    }

    #[test]
    fn extract_tool_calls_from_text_single_call() {
        let text = r#"<tool_call>{"name":"search","arguments":{"q":"weather"}}</tool_call>"#;
        let (cleaned, calls) = convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "search");
        assert!(cleaned.is_empty());
    }

    #[test]
    fn extract_tool_calls_from_text_with_id() {
        let text = r#"<tool_call>{"name":"read_file","arguments":{"path":"/etc/hosts"},"id":"call_abc"}</tool_call>"#;
        let (_, calls) = convert_xml_tool_calls(text, true);
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
        let (_, calls) = convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "complex");
    }
}
