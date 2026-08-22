//! Direct DeepSeek API client.
//!
//! Bypasses the chat UI and calls DeepSeek's internal endpoints directly,
//! using the warmed browser only for authenticated cookies and the bearer
//! token imported from Firefox.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream::{BoxStream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

fn citation_marker_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\[citation:\d+\]$").unwrap())
}

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatMessageDelta, ChunkChoice, Citation, Tool, ToolCall, Usage,
};
use crate::providers::retry_after_from_headers;
use crate::providers::tokenizer::estimate_tokens;
use crate::session::SessionHandle;

use crate::providers::mtp;
use crate::providers::solver::{PoWHeader, SolverChain, SolverRegistry};
use crate::providers::tool_call::{
    convert_xml_tool_calls, inject_tool_prompt,
    XmlToolCallStripper,
};
#[cfg(test)]
use crate::providers::tool_call::parse_tool_calls_from_content;
use crate::providers::send_with_retry;
use super::state::{ensure_model_matches, SessionStore};
use super::upload::FileUploadService;
use super::url::{build_session_url, new_session_token, parse_session_url};

const BASE_URL: &str = "https://chat.deepseek.com";
const COMPLETION_PATH: &str = "/api/v0/chat/completion";
const APP_VERSION: &str = "2.2.0";
const CLIENT_VERSION: &str = "2.2.0";

/// Maps public model IDs to DeepSeek's internal `model_type` wire value.
/// The V4/R1 aliases follow the API model roster in docs/LatestAImodels:
/// V4-Pro and R1 are the flagship reasoning models (expert wire type),
/// V4-Flash and V3.2 are the fast/cheaper chat models (default wire type).
fn model_type(model_id: &str) -> Option<&'static str> {
    match model_id {
        "deepseek-chat" | "deepseek-instant" | "deepseek-v4-flash" | "deepseek-v3.2" => {
            Some("default")
        }
        "deepseek-reasoner" | "deepseek-expert" | "deepseek-v4-pro" | "deepseek-r1" => {
            Some("expert")
        }
        "deepseek-vision" => Some("vision"),
        _ => None,
    }
}

/// Whether a model reasons by default. Reasoner models always emit THINK
/// fragments in the web app and the official API, so default thinking on
/// unless the request explicitly disables it.
fn default_thinking(model_id: &str) -> bool {
    model_type(model_id) == Some("expert")
}

/// Effective internal model type, considering image auto-detection.
fn effective_model_type(model_id: &str, has_images: bool) -> Option<&'static str> {
    if has_images || model_id == "deepseek-vision" {
        Some("vision")
    } else {
        model_type(model_id)
    }
}

/// Which logical stream a DeepSeek fragment belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FragmentKind {
    Content,
    Reasoning,
}

/// A single direct-API request context.
pub struct DirectClient {
    http: reqwest::Client,
    #[allow(dead_code)]
    session: SessionHandle,
    solvers: SolverRegistry,
    solver_chain: SolverChain,
    model_id: String,
    store: SessionStore,
}

impl DirectClient {
    /// Create a client for one request.
    ///
    /// `solvers` should contain at least one solver matching an entry in
    /// `solver_chain`, otherwise PoW challenge solving will fail.
    pub fn new(
        session: SessionHandle,
        solvers: SolverRegistry,
        solver_chain: SolverChain,
        model_id: &str,
        store: SessionStore,
    ) -> Result<Self, GatewayError> {
        let token = session
            .local_storage
            .iter()
            .find_map(|e| e.as_deepseek_token())
            .ok_or_else(|| {
                GatewayError::Auth(
                    "DeepSeek bearer token not found. Import a logged-in chat.deepseek.com \
                     session (localStorage `userToken`) and retry."
                        .to_string(),
                )
            })?;

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| GatewayError::Internal(format!("invalid bearer token: {e}")))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("accept", HeaderValue::from_static("*/*"));
        headers.insert("origin", HeaderValue::from_static(BASE_URL));
        headers.insert("referer", HeaderValue::from_static("https://chat.deepseek.com/"));
        headers.insert("x-app-version", HeaderValue::from_static(APP_VERSION));
        headers.insert("x-client-version", HeaderValue::from_static(CLIENT_VERSION));
        headers.insert("x-client-platform", HeaderValue::from_static("web"));
        headers.insert("x-client-locale", HeaderValue::from_static("en_US"));
        headers.insert(
            "x-client-bundle-id",
            HeaderValue::from_static("com.deepseek.chat"),
        );
        headers.insert("x-client-timezone-offset", HeaderValue::from_static("19800"));

        let user_agent = HeaderValue::from_str(&session.user_agent)
            .unwrap_or_else(|_| HeaderValue::from_static("Mozilla/5.0"));
        headers.insert(reqwest::header::USER_AGENT, user_agent);

        let cookie_header = session
            .cookie_jar
            .get_cookie_header(&url::Url::parse(BASE_URL).expect("hard-coded deepseek url"));
        if !cookie_header.is_empty() {
            headers.insert(
                HeaderName::from_static("cookie"),
                HeaderValue::from_str(&cookie_header)
                    .map_err(|e| GatewayError::Internal(format!("invalid cookie header: {e}")))?,
            );
        }

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .default_headers(headers)
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            http,
            session,
            solvers,
            solver_chain,
            model_id: model_id.to_string(),
            store,
        })
    }

    /// Non-streaming chat completion.
    pub async fn chat(self, request: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        let model = request.model.clone();

        let mut text = String::new();
        let mut reasoning_text = String::new();
        let mut citations: Vec<Citation> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut session_url: Option<String> = None;
        let store = self.store.clone();
        let stream_request = request.clone();
        let mut stream = self.chat_stream(stream_request).await?;
        while let Some(result) = stream.next().await {
            let chunk = result?;
            if session_url.is_none() && chunk.session_url.is_some() {
                session_url = chunk.session_url.clone();
            }
            for choice in &chunk.choices {
                if let Some(ref content) = choice.delta.content {
                    text.push_str(content);
                }
                if let Some(ref reasoning) = choice.delta.reasoning_content {
                    reasoning_text.push_str(reasoning);
                }
                if let Some(ref new_citations) = choice.delta.citations {
                    for c in new_citations {
                        if !citations.iter().any(|existing| existing.url == c.url && existing.index == c.index) {
                            citations.push(c.clone());
                        }
                    }
                }
                if let Some(ref new_tool_calls) = choice.delta.tool_calls {
                    for tc in new_tool_calls {
                        if !tool_calls.iter().any(|existing| existing.id == tc.id) {
                            tool_calls.push(tc.clone());
                        }
                    }
                }
            }
        }

        // DeepSeek returns inline citation markers such as [citation:2]. Strip
        // them from the final text because the citations are exposed separately.
        let mut text = citation_marker_regex()
            .replace_all(&text, "")
            .to_string();

        // The internal endpoint ignores native `tools`. If the user supplied
        // tools, try to parse any <tool_call> markers the model emitted.
        if request.tools.is_some() {
            let (cleaned_text, parsed) = convert_xml_tool_calls(&text, true);
            if let Some(calls) = parsed {
                tool_calls = calls;
                text = cleaned_text;
            }
        }

        // Remember the assistant's tool calls so a later `role: "tool"` turn
        // can reference the exact call that was made.
        if !tool_calls.is_empty() {
            if let Some(ref url) = session_url {
                if let Ok(session_id) = parse_session_url(url) {
                    store.store_tool_calls(&session_id, &tool_calls).await;
                }
            }
        }

        let finish_reason = if tool_calls.is_empty() {
            "stop".to_string()
        } else {
            "tool_calls".to_string()
        };
        let prompt_text: String = request.messages.iter().map(|m| m.content.as_text()).collect();
        let completion_text = format!("{}{}", text, reasoning_text);
        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: model.clone(),
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: crate::models::ChatContent::String(text),
                    name: None,
                    reasoning_content: if !reasoning_text.is_empty() {
                        Some(reasoning_text)
                    } else {
                        None
                    },
                    citations: if citations.is_empty() { None } else { Some(citations) },
                    tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
                    tool_call_id: None,
                },
                finish_reason,
            }],
            usage: Usage {
                prompt_tokens: estimate_tokens("deepseek", &model, &prompt_text),
                completion_tokens: estimate_tokens("deepseek", &model, &completion_text),
                total_tokens: estimate_tokens("deepseek", &model, &prompt_text)
                    + estimate_tokens("deepseek", &model, &completion_text),
            },
            session_url,
        })
    }

    /// Streaming chat completion.
    pub async fn chat_stream(
        self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let model = request.model.clone();
        let requested_session_url = request.session_url.clone();
        let thinking_enabled = request.thinking.unwrap_or(default_thinking(&request.model));
        let search_enabled = request.search.unwrap_or(false);
        let tools_enabled = request.tools.is_some();

        // Resolve an existing branch or create a new chat session.
        //
        // The client's session_url carries an opaque branch token, not the
        // raw DeepSeek chat session id. Each completed turn mints a fresh
        // token mapped to `(chat_session_id, message_id)`, so two concurrent
        // continuations from the same parent each get their own continuable
        // branch instead of a single last-writer-wins tip (message-tree
        // semantics).
        let incoming_token: Option<String> = match &requested_session_url {
            Some(url) => Some(parse_session_url(url)?),
            None => None,
        };

        let (chat_session_id, parent_message_id) = if let Some(ref token) = incoming_token {
            let (chat_id, parent_id, tracked_model) = self
                .store
                .acquire(token)
                .await
                .ok_or_else(|| GatewayError::BadRequest(format!("invalid or expired session_url: {url}", url = requested_session_url.as_deref().unwrap_or_default())))?;
            ensure_model_matches(&request.model, &tracked_model)?;
            (chat_id, Some(parent_id))
        } else {
            let id = self.create_chat_session().await?;
            (id, None)
        };

        // Fresh branch token for this turn. The session_url emitted on every
        // chunk is built from it so a client that continues from the stream
        // always follows exactly this branch.
        let branch_token = new_session_token();
        let session_url = Some(build_session_url(&branch_token));

        // Resolve and upload any image/file parts in the last user message.
        let upload_service = FileUploadService::new(
            self.http.clone(),
            self.session.clone(),
            self.solvers.clone(),
            self.solver_chain.clone(),
            self.store.upload_cache.clone(),
        );
        let document_model_type = model_type(&request.model).unwrap_or("default");
        let (prompt, file_ids) = upload_service
            .prepare_last_user_message(
                incoming_token.as_deref().unwrap_or_default(),
                &request.messages,
                document_model_type,
                thinking_enabled,
                &self.store,
            )
            .await?;

        // The internal DeepSeek endpoint does not natively support function
        // calling. When tools are requested, inject the MTP/1 system prompt
        // into the prompt and later parse the model's MTP tool blocks.
        //
        // On tool-result follow-ups (last message is role "tool"), the prompt
        // already contains the formatted tool output. Re-injecting tool
        // definitions confuses the model with a redundant header. Only inject
        // on fresh user turns.
        let last_is_tool = request.messages.last().map_or(false, |m| m.role == "tool");
        let prompt = if let Some(ref tools) = request.tools {
            if last_is_tool {
                // Follow-up: include the user's original question for context,
                // then the tool results. Skip re-injecting tool definitions.
                let user_question = request.messages.iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.as_text())
                    .unwrap_or_default();
                let p = if user_question.is_empty() {
                    prompt
                } else {
                    format!("User request:\n{user_question}\n\n{prompt}")
                };
                tracing::info!(prompt_len = p.len(), tools_count = tools.len(), last_is_tool, "DeepSeek prompt (follow-up, no re-inject)");
                tracing::debug!(prompt = %p, "Full DeepSeek follow-up prompt sent to upstream");
                p
            } else {
                let mtp_prompt = mtp::build_mtp_system_prompt(tools, request.tool_choice.as_ref(), false);
                let p = format!("{mtp_prompt}\n\nUser request:\n{prompt}");
                tracing::info!(prompt_len = p.len(), tools_count = tools.len(), "DeepSeek prompt after MTP injection");
                tracing::debug!(prompt = %p, "Full DeepSeek prompt sent to upstream");
                p
            }
        } else {
            tracing::info!(prompt_len = prompt.len(), "DeepSeek prompt (no tools)");
            prompt
        };

        let pow_header = create_pow_header_for(&self.http, &self.solvers, &self.solver_chain, COMPLETION_PATH).await?;
        let hif_leim = self.fetch_hif_leim().await.ok();
        let body = self.build_completion_body(
            &chat_session_id,
            parent_message_id,
            &prompt,
            &file_ids,
            &request,
        );
        // Dump full body to file so we can inspect the exact prompt sent to upstream
    if let Ok(json) = serde_json::to_string_pretty(&body) {
        let _ = std::fs::write("/tmp/deepseek_upstream_body.json", &json);
    }

        let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let model_for_store = self.model_id.clone();

        let http = self.http;
        let url = format!("{BASE_URL}{COMPLETION_PATH}");
        let mut request_builder = http.post(&url).json(&body);
        request_builder = request_builder.header(&pow_header.name, &pow_header.value);
        if let Some(token) = hif_leim {
            request_builder = request_builder.header("x-hif-leim", token);
        }

        let store = self.store.clone();
        let branch_token_for_store = branch_token.clone();
        let chat_session_id_for_store = chat_session_id.clone();

        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

        tokio::spawn(async move {
            // No per-session lock held - allow concurrent requests. Branch
            // identity comes from the fresh token minted above, not a lock.

            let resp = match send_with_retry(request_builder).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(GatewayError::Provider(format!(
                        "completion request failed: {e}"
                    ))));
                    return;
                }
            };

            let tools_for_stream = request.tools.clone().unwrap_or_default();
            let (assistant_id, saw_tool_calls, tool_calls) = match handle_completion_stream(
                resp,
                &model,
                &id_prefix,
                session_url.clone(),
                search_enabled,
                tools_enabled,
                &tools_for_stream,
                tx.clone(),
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };

            // Store the completed turn under this branch's fresh token. The
            // real DeepSeek chat session id lives inside the stored data so a
            // continuation can resume the same chat thread.
            if let Some(id) = assistant_id {
                store
                    .insert(
                        branch_token_for_store.clone(),
                        model_for_store,
                        chat_session_id_for_store.clone(),
                        id,
                    )
                    .await;
            }

            if !tool_calls.is_empty() {
                store
                    .store_tool_calls(&branch_token_for_store, &tool_calls)
                    .await;
            }

            let finish_reason = if saw_tool_calls {
                "tool_calls".to_string()
            } else {
                "stop".to_string()
            };
            let _ = tx.send(Ok(ChatCompletionChunk {
                id: format!("{}-final", id_prefix),
                object: "chat.completion.chunk".to_string(),
                created: current_timestamp(),
                model: model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChatMessageDelta::default(),
                    finish_reason: Some(finish_reason),
                }],
                session_url: session_url.clone(),
            }));
        });

        Ok(UnboundedReceiverStream::new(rx).boxed())
    }

    async fn create_chat_session(&self) -> Result<String, GatewayError> {
        let url = format!("{BASE_URL}/api/v0/chat_session/create");
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("chat_session/create request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| GatewayError::Provider(format!("chat_session/create decode failed: {e}")))?;

        let biz = unwrap_biz(body, status)?;
        biz.get("chat_session")
            .and_then(|s| s.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| GatewayError::Provider("chat_session/create returned no session id".to_string()))
    }

    /// Fetch the optional `x-hif-leim` token used for some model endpoints.
    async fn fetch_hif_leim(&self) -> Result<String, GatewayError> {
        let url = "https://hif-leim.deepseek.com/query";
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| GatewayError::Provider(format!("hif-leim request failed: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| GatewayError::Provider(format!("hif-leim decode failed: {e}")))?;
        tracing::debug!(status = %status, body = %text, "hif-leim response");

        if !status.is_success() {
            return Err(GatewayError::Provider(format!(
                "hif-leim returned {status}: {text}"
            )));
        }

        // Try the standard biz_data envelope first, then fall back to raw body.
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(biz) = obj.get("data").and_then(|d| d.get("biz_data")) {
                if let Some(s) = biz.as_str() {
                    return Ok(s.to_string());
                }
            }
        }

        Ok(text.trim().to_string())
    }

    fn build_completion_body(
        &self,
        session_id: &str,
        parent_message_id: Option<i64>,
        prompt: &str,
        file_ids: &[String],
        request: &ChatCompletionRequest,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "chat_session_id": session_id,
            "parent_message_id": parent_message_id,
            "prompt": prompt,
            "ref_file_ids": file_ids,
            "thinking_enabled": request.thinking.unwrap_or(default_thinking(&request.model)),
            "search_enabled": request.search.unwrap_or(false),
            "action": serde_json::Value::Null,
            "preempt": false,
        });

        // A DeepSeek thread's model is fixed at creation. Only send model_type
        // on the first message of a thread.
        if parent_message_id.is_none() {
            if let Some(model_type) = effective_model_type(&request.model, !file_ids.is_empty()) {
                body["model_type"] = serde_json::Value::String(model_type.to_string());
            }
        }

        // NOTE: Do NOT forward tools/tool_choice in the body. DeepSeek's
        // internal web endpoint does not support native tool calling; the
        // fields are silently ignored. Including them adds noise to the
        // request and can confuse the model.

        if let Some(t) = request.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(m) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(m);
        }
        if let Some(p) = request.top_p {
            body["top_p"] = serde_json::json!(p);
        }
        if let Some(ref s) = request.stop {
            body["stop"] = serde_json::json!(s);
        }
        if let Some(p) = request.presence_penalty {
            body["presence_penalty"] = serde_json::json!(p);
        }
        if let Some(f) = request.frequency_penalty {
            body["frequency_penalty"] = serde_json::json!(f);
        }

        // Forward native JSON mode if provided (DeepSeek web API supports it).
        if let Some(ref fmt) = request.response_format {
            if fmt.r#type == "json_object" {
                body["response_format"] = serde_json::json!({
                    "type": "json_object"
                });
            }
        }

        body
    }
}

async fn create_pow_header_for(
    http: &reqwest::Client,
    solvers: &SolverRegistry,
    chain: &SolverChain,
    target_path: &str,
) -> Result<PoWHeader, GatewayError> {
    let url = format!("{BASE_URL}/api/v0/chat/create_pow_challenge");
    let resp = http
        .post(&url)
        .json(&serde_json::json!({"target_path": target_path}))
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("create_pow_challenge request failed: {e}")))?;

    let status = resp.status();
    let body = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| GatewayError::Provider(format!("create_pow_challenge decode failed: {e}")))?;

    let wrapper = unwrap_biz(body, status)?;
    tracing::debug!(challenge = %wrapper, "PoW challenge received");
    let challenge = wrapper.get("challenge").ok_or_else(|| {
        GatewayError::Provider("create_pow_challenge response missing challenge object".to_string())
    })?;
    solvers.solve_fallback(chain, challenge)
}

async fn handle_completion_stream(
    resp: reqwest::Response,
    model: &str,
    id_prefix: &str,
    session_url: Option<String>,
    search_enabled: bool,
    tools_enabled: bool,
    tools: &[Tool],
    tx: mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
) -> Result<(Option<i64>, bool, Vec<ToolCall>), GatewayError> {
    let status = resp.status();
    let headers = resp.headers().clone();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    tracing::debug!(status = %status, content_type = %content_type, "completion response headers");

    if content_type.contains("application/json") {
        let body_text = resp.text().await.unwrap_or_default();
        tracing::debug!(body = %body_text, "completion JSON response body");
        let parsed: serde_json::Value = match serde_json::from_str(&body_text) {
            Ok(v) => v,
            Err(e) => {
                return Err(GatewayError::Provider(format!(
                    "DeepSeek returned non-JSON error ({status}): {body_text} (parse: {e})"
                )));
            }
        };
        let code = parsed.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = parsed
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(GatewayError::Provider(format!(
                "DeepSeek API error (code={code}): {msg}"
            )));
        }
        if let Some(biz) = parsed.get("data").and_then(|d| d.get("biz_data")) {
            tracing::warn!(
                "DeepSeek returned JSON with biz_data instead of SSE; content={}",
                serde_json::to_string(biz).unwrap_or_default()
            );
        }
        return Ok((None, false, Vec::new()));
    }

    if !status.is_success() {
        let text = resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", status));
        if status.as_u16() == 429 {
            return Err(GatewayError::ProviderRateLimited {
                message: format!("completion rate limited (429): {text}"),
                retry_after: retry_after_from_headers(&headers),
            });
        }
        return Err(GatewayError::Provider(format!(
            "completion returned error {status}: {text}"
        )));
    }

    let mut counter: u32 = 0;
    let mut state = StreamState::new(tools);
    let mut assistant_message_id: Option<i64> = None;
    let tools_enabled = tools_enabled;

    let mut stream = resp.bytes_stream();
    let mut buffer = Vec::new();

    while let Some(result) = stream.next().await {
        let bytes = result.map_err(|e| GatewayError::Provider(format!("stream read error: {e}")))?;
        buffer.extend_from_slice(&bytes);

        // Extract complete lines, keeping partial bytes in the buffer.
        let mut consumed = 0;
        for (i, window) in buffer.windows(2).enumerate() {
            if window == b"\n\n" || window == b"\r\n" {
                let line = &buffer[consumed..i];
                if search_enabled {
                    tracing::trace!(line = %String::from_utf8_lossy(line), "upstream sse line");
                }
                if let Some((chunks, msg_id)) = parse_sse_line(
                    line,
                    model,
                    id_prefix,
                    &session_url,
                    search_enabled,
                    tools_enabled,
                    &mut counter,
                    &mut state,
                ) {
                    if let Some(id) = msg_id {
                        assistant_message_id = Some(id);
                    }
                    for chunk in chunks {
                        if tx.send(Ok(chunk)).is_err() {
                            return Ok((assistant_message_id, state.saw_tool_calls, state.collected_tool_calls));
                        }
                    }
                }
                consumed = i + 2;
            }
        }
        buffer.drain(0..consumed);
    }

    // Process any trailing line without a trailing blank line.
    if !buffer.is_empty() {
        if let Some((chunks, msg_id)) = parse_sse_line(
            &buffer,
            model,
            id_prefix,
            &session_url,
            search_enabled,
            tools_enabled,
            &mut counter,
            &mut state,
        ) {
            if let Some(id) = msg_id {
                assistant_message_id = Some(id);
            }
            for chunk in chunks {
                let _ = tx.send(Ok(chunk));
            }
        }
    }

    // Flush any pending partial MTP tool block when the stream ends.
    state.mtp_state.finish(&state.tools);
    if state.mtp_state.saw_tool_calls {
        state.saw_tool_calls = true;
        state.collected_tool_calls.extend(state.mtp_state.collected_tool_calls.clone());
        state.mtp_state.collected_tool_calls.clear();
    }

    // Emit any tool calls collected during the stream as a single chunk.
    if !state.collected_tool_calls.is_empty() {
        counter += 1;
        let role = if !state.emitted_role {
            state.emitted_role = true;
            Some("assistant".to_string())
        } else {
            None
        };
        let _ = tx.send(Ok(build_tool_calls_chunk(
            model,
            id_prefix,
            counter,
            role,
            state.collected_tool_calls.clone(),
            &session_url,
        )));
    }

    Ok((assistant_message_id, state.saw_tool_calls, state.collected_tool_calls))
}

struct StreamState {
    /// Last active append path, used for bare append frames.
    active_path: Option<String>,
    /// Maps a fragment base path (without `/content` or `/message_id`) to its kind.
    path_kinds: HashMap<String, FragmentKind>,
    /// Whether the assistant role has already been emitted.
    emitted_role: bool,
    /// Current number of fragments in the response, used to keep negative-index
    /// path mappings correct after fragment appends.
    fragment_count: usize,
    /// Whether the stream contained tool calls.
    saw_tool_calls: bool,
    /// MTP/1 streaming state (parses `[MIRAGE_TOOL_CALL_V1]` blocks).
    mtp_state: mtp::MtpStreamState,
    /// Tool calls collected from the stream, emitted as a single chunk at the end.
    collected_tool_calls: Vec<ToolCall>,
    /// The tool definitions for this request (used to validate MTP blocks).
    tools: Vec<Tool>,
}

impl StreamState {
    fn new(tools: &[Tool]) -> Self {
        Self {
            active_path: None,
            path_kinds: HashMap::new(),
            emitted_role: false,
            fragment_count: 0,
            saw_tool_calls: false,
            mtp_state: mtp::MtpStreamState::new(),
            collected_tool_calls: Vec::new(),
            tools: tools.to_vec(),
        }
    }
}

/// Process a content delta through the MTP tool block parser, returning
/// the plain text that should be emitted to the client. Text inside
/// `[MIRAGE_TOOL_CALL_V1]` markers is absorbed and converted to structured
/// tool calls.
fn process_content_text(content: &str, state: &mut StreamState) -> String {
    let clean = state.mtp_state.process_delta(content, &state.tools);
    if state.mtp_state.saw_tool_calls {
        state.saw_tool_calls = true;
        state.collected_tool_calls.extend(state.mtp_state.collected_tool_calls.clone());
        state.mtp_state.collected_tool_calls.clear();
    }
    clean
}

/// Emit a content fragment through the tool-call state machine. Returns any
/// chunks that should be sent to the client immediately.
fn emit_content(
    content: &str,
    state: &mut StreamState,
    model: &str,
    id_prefix: &str,
    session_url: &Option<String>,
    counter: &mut u32,
    kind: FragmentKind,
    tools_enabled: bool,
) -> Vec<ChatCompletionChunk> {
    // Citation markers such as [citation:2] are surfaced separately; skip a
    // fragment that is only a marker.
    if kind == FragmentKind::Content && citation_marker_regex().is_match(content) {
        return Vec::new();
    }

    let emit_text = if tools_enabled && kind == FragmentKind::Content {
        process_content_text(content, state)
    } else {
        content.to_string()
    };

    if emit_text.is_empty() {
        return Vec::new();
    }

    *counter += 1;
    let role = if !state.emitted_role {
        state.emitted_role = true;
        Some("assistant".to_string())
    } else {
        None
    };
    vec![build_chunk(
        model,
        id_prefix,
        *counter,
        role,
        &emit_text,
        session_url,
        kind,
    )]
}

fn parse_sse_line(
    line: &[u8],
    model: &str,
    id_prefix: &str,
    session_url: &Option<String>,
    search_enabled: bool,
    tools_enabled: bool,
    counter: &mut u32,
    state: &mut StreamState,
) -> Option<(Vec<ChatCompletionChunk>, Option<i64>)> {
    let line = match std::str::from_utf8(line) {
        Ok(s) => s,
        Err(_) => return None,
    }
    .trim();

    if !line.starts_with("data:") {
        return None;
    }

    let payload = line["data:".len()..].trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }

    let obj: serde_json::Value = serde_json::from_str(payload).ok()?;

    // Snapshot frame: full response object.
    if let Some(v) = obj.get("v").and_then(|v| v.as_object()) {
        if v.contains_key("response") {
            tracing::debug!(
                raw_snapshot = %serde_json::to_string(&serde_json::Value::Object(v.clone())).unwrap_or_default(),
                "deepseek response snapshot"
            );
            let msg_id = capture_message_id(v);
            let mut chunks = Vec::new();

            if search_enabled {
                log_search_snapshot(v);
                if let Some(citations) = extract_citations(v) {
                    *counter += 1;
                    chunks.push(build_citation_chunk(
                        model,
                        id_prefix,
                        *counter,
                        citations,
                        session_url,
                    ));
                }
            }

            if tools_enabled {
                if let Some(tool_calls) = extract_tool_calls(v) {
                    state.saw_tool_calls = true;
                    state.collected_tool_calls.extend(tool_calls.clone());
                    *counter += 1;
                    let role = if !state.emitted_role {
                        state.emitted_role = true;
                        Some("assistant".to_string())
                    } else {
                        None
                    };
                    chunks.push(build_tool_calls_chunk(
                        model,
                        id_prefix,
                        *counter,
                        role,
                        tool_calls,
                        session_url,
                    ));
                }
            }

            let fragments = v
                .get("response")
                .and_then(|r| r.get("fragments"))
                .and_then(|f| f.as_array());
            if let Some(fragments) = fragments {
                let len = fragments.len();
                state.fragment_count = len;
                for (i, frag) in fragments.iter().enumerate() {
                    let typ = frag.get("type").and_then(|t| t.as_str())?;
                    let content = frag.get("content").and_then(|c| c.as_str());
                    let kind = match typ {
                        "RESPONSE" | "TEMPLATE_RESPONSE" => Some(FragmentKind::Content),
                        "THINK" => Some(FragmentKind::Reasoning),
                        "SEARCH" | "TOOL_SEARCH" => {
                            if search_enabled {
                                if let Some(citations) = extract_citations_from_fragment(frag) {
                                    *counter += 1;
                                    chunks.push(build_citation_chunk(
                                        model,
                                        id_prefix,
                                        *counter,
                                        citations,
                                        session_url,
                                    ));
                                }
                            }
                            None
                        }
                        _ => None,
                    };

                    if let Some(kind) = kind {
                        let base_pos = format!("response/fragments/{i}");
                        let base_neg = format!("response/fragments/{}", i as isize - len as isize);
                        state.path_kinds.insert(base_pos, kind);
                        state.path_kinds.insert(base_neg.clone(), kind);
                        state.active_path = Some(format!("{base_neg}/content"));

                        if let Some(content) = content {
                            chunks.extend(emit_content(
                                content,
                                state,
                                model,
                                id_prefix,
                                session_url,
                                counter,
                                kind,
                                tools_enabled,
                            ));
                        }
                    }
                }
            }

            return Some((chunks, msg_id));
        }
    }

    // Path-setting append frame.
    if let Some(path) = obj.get("p").and_then(|p| p.as_str()) {
        state.active_path = Some(path.to_string());

        if path.ends_with("message_id") {
            if let Some(id) = obj.get("v").and_then(|v| v.as_i64()) {
                return Some((Vec::new(), Some(id)));
            }
        }

        if let Some(v) = obj.get("v") {
            if path.ends_with("content") {
                if let Some(s) = v.as_str() {
                    let kind = kind_for_path(path, &state.path_kinds).unwrap_or(FragmentKind::Content);
                    let chunks = emit_content(
                        s,
                        state,
                        model,
                        id_prefix,
                        session_url,
                        counter,
                        kind,
                        tools_enabled,
                    );
                    return Some((chunks, None));
                }
            } else if search_enabled && (path.ends_with("results") || path.ends_with("references") || path.ends_with("citations")) {
                if let Some(citations) = extract_citations_from_fragment(v) {
                    *counter += 1;
                    return Some((
                        vec![build_citation_chunk(
                            model,
                            id_prefix,
                            *counter,
                            citations,
                            session_url,
                        )],
                        None,
                    ));
                }
            }
        }

        let op = obj.get("o").and_then(|o| o.as_str());
        if op == Some("APPEND") && path == "response/fragments" {
            if let Some(arr) = obj.get("v").and_then(|v| v.as_array()) {
                // Shift existing negative index mappings to make room for the new
                // fragment at -1.
                let neg1 = state.path_kinds.remove("response/fragments/-1");
                if let Some(kind) = neg1 {
                    state.path_kinds.insert("response/fragments/-2".to_string(), kind);
                }
                state.fragment_count += arr.len();
                if let Some(last) = arr.last() {
                    if let Some(kind) = fragment_kind(last) {
                        state.path_kinds.insert("response/fragments/-1".to_string(), kind);
                        state.active_path = Some("response/fragments/-1/content".to_string());
                    }
                }

                let mut chunks = Vec::new();
                for frag in arr {
                    if let Some(kind) = fragment_kind(frag) {
                        if let Some(content) = frag.get("content").and_then(|c| c.as_str()) {
                            chunks.extend(emit_content(
                                content,
                                state,
                                model,
                                id_prefix,
                                session_url,
                                counter,
                                kind,
                                tools_enabled,
                            ));
                        }
                    }
                }
                if !chunks.is_empty() {
                    return Some((chunks, None));
                }
            }
            return Some((Vec::new(), None));
        }
        return Some((Vec::new(), None));
    }

    // Bare append to the current path.
    if let Some(v) = obj.get("v").and_then(|v| v.as_str()) {
        if let Some(ref path) = state.active_path {
            if path.ends_with("content") {
                let kind = kind_for_path(path, &state.path_kinds).unwrap_or(FragmentKind::Content);
                let chunks = emit_content(
                    v,
                    state,
                    model,
                    id_prefix,
                    session_url,
                    counter,
                    kind,
                    tools_enabled,
                );
                return Some((chunks, None));
            }
        }
    }

    None
}

fn fragment_kind(frag: &serde_json::Value) -> Option<FragmentKind> {
    let typ = frag.get("type").and_then(|t| t.as_str())?;
    match typ {
        "RESPONSE" | "TEMPLATE_RESPONSE" => Some(FragmentKind::Content),
        "THINK" => Some(FragmentKind::Reasoning),
        _ => None,
    }
}

fn kind_for_path(path: &str, kinds: &HashMap<String, FragmentKind>) -> Option<FragmentKind> {
    let prefix = path
        .trim_end_matches("/content")
        .trim_end_matches("/message_id");
    kinds.get(prefix).copied()
}

fn capture_message_id(snapshot: &serde_json::Map<String, serde_json::Value>) -> Option<i64> {
    if let Some(resp) = snapshot.get("response").and_then(|r| r.as_object()) {
        if let Some(id) = resp
            .get("message_id")
            .and_then(|v| v.as_i64())
            .or_else(|| resp.get("id").and_then(|v| v.as_i64()))
        {
            return Some(id);
        }
    }
    snapshot
        .get("message_id")
        .and_then(|v| v.as_i64())
        .or_else(|| snapshot.get("id").and_then(|v| v.as_i64()))
}

fn build_chunk(
    model: &str,
    id_prefix: &str,
    counter: u32,
    role: Option<String>,
    content: &str,
    session_url: &Option<String>,
    kind: FragmentKind,
) -> ChatCompletionChunk {
    let mut delta = ChatMessageDelta {
        role,
        content: None,
        reasoning_content: None,
        citations: None,
        tool_calls: None,
    };
    match kind {
        FragmentKind::Content => delta.content = Some(content.to_string()),
        FragmentKind::Reasoning => delta.reasoning_content = Some(content.to_string()),
    }

    ChatCompletionChunk {
        id: format!("{}-{}", id_prefix, counter),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: None,
        }],
        session_url: session_url.clone(),
    }
}

fn build_citation_chunk(
    model: &str,
    id_prefix: &str,
    counter: u32,
    citations: Vec<Citation>,
    session_url: &Option<String>,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: format!("{}-{}", id_prefix, counter),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta {
                role: None,
                content: None,
                reasoning_content: None,
                citations: Some(citations),
                tool_calls: None,
            },
            finish_reason: None,
        }],
        session_url: session_url.clone(),
    }
}

fn build_tool_calls_chunk(
    model: &str,
    id_prefix: &str,
    counter: u32,
    role: Option<String>,
    tool_calls: Vec<ToolCall>,
    session_url: &Option<String>,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: format!("{}-{}", id_prefix, counter),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta {
                role,
                content: None,
                reasoning_content: None,
                citations: None,
                tool_calls: Some(tool_calls),
            },
            finish_reason: None,
        }],
        session_url: session_url.clone(),
    }
}

/// Log the full snapshot when search is enabled so we can discover the exact
/// citation shape from live traffic.
fn log_search_snapshot(snapshot: &serde_json::Map<String, serde_json::Value>) {
    let raw = serde_json::to_string(&serde_json::Value::Object(snapshot.clone())).unwrap_or_default();
    tracing::debug!(raw_snapshot = %raw, "search-enabled response snapshot for citation discovery");
}

/// Best-effort citation extraction from a snapshot response object.
fn extract_citations(snapshot: &serde_json::Map<String, serde_json::Value>) -> Option<Vec<Citation>> {
    if let Some(response) = snapshot.get("response") {
        for key in ["search_results", "citations", "references", "results"] {
            if let Some(arr) = response.get(key).and_then(|v| v.as_array()) {
                let citations: Vec<_> = arr
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| parse_citation_object(v, Some(i as i64 + 1)))
                    .collect();
                if !citations.is_empty() {
                    return Some(citations);
                }
            }
        }
    }
    None
}

/// Best-effort citation extraction from a single search fragment.
fn extract_citations_from_fragment(frag: &serde_json::Value) -> Option<Vec<Citation>> {
    // The fragment itself might be an array/object containing citation data.
    if let Some(arr) = frag.as_array() {
        let citations: Vec<_> = arr
            .iter()
            .enumerate()
            .filter_map(|(i, v)| parse_citation_object(v, Some(i as i64 + 1)))
            .collect();
        if !citations.is_empty() {
            return Some(citations);
        }
    }
    if let Some(obj) = frag.as_object() {
        for key in ["results", "citations", "search_results", "references"] {
            if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
                let citations: Vec<_> = arr
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| parse_citation_object(v, Some(i as i64 + 1)))
                    .collect();
                if !citations.is_empty() {
                    return Some(citations);
                }
            }
        }
        // Maybe the fragment itself is a single citation.
        if let Some(c) = parse_citation_object(&serde_json::Value::Object(obj.clone()), None) {
            return Some(vec![c]);
        }
    }
    None
}

fn parse_citation_object(value: &serde_json::Value, fallback_index: Option<i64>) -> Option<Citation> {
    let obj = value.as_object()?;

    let url = obj
        .get("url")
        .or_else(|| obj.get("link"))
        .or_else(|| obj.get("href"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let title = obj
        .get("title")
        .or_else(|| obj.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let snippet = obj
        .get("snippet")
        .or_else(|| obj.get("summary"))
        .or_else(|| obj.get("content"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let index = obj
        .get("cite_index")
        .and_then(|v| v.as_i64())
        .or_else(|| obj.get("index").and_then(|v| v.as_i64()))
        .or(fallback_index);

    if url.is_none() && title.is_none() && snippet.is_none() {
        return None;
    }

    Some(Citation {
        index,
        title,
        url,
        snippet,
        start_ix: None,
        end_ix: None,
    })
}

/// Best-effort tool-call extraction from a snapshot response object.
fn extract_tool_calls(snapshot: &serde_json::Map<String, serde_json::Value>) -> Option<Vec<ToolCall>> {
    // First look for a top-level `tool_calls` array inside the response.
    if let Some(response) = snapshot.get("response").and_then(|r| r.as_object()) {
        if let Some(arr) = response.get("tool_calls").and_then(|v| v.as_array()) {
            let calls: Vec<_> = arr.iter().filter_map(parse_tool_call_object).collect();
            if !calls.is_empty() {
                return Some(calls);
            }
        }
    }

    // Also walk fragments for TOOL_CALL typed entries.
    if let Some(fragments) = snapshot
        .get("response")
        .and_then(|r| r.get("fragments"))
        .and_then(|f| f.as_array())
    {
        let mut calls = Vec::new();
        for frag in fragments {
            if let Some(typ) = frag.get("type").and_then(|t| t.as_str()) {
                if typ.eq_ignore_ascii_case("tool_call") {
                    if let Some(tc) = parse_tool_call_object(frag) {
                        calls.push(tc);
                    }
                }
            }
            if let Some(arr) = frag.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in arr {
                    if let Some(tc) = parse_tool_call_object(tc) {
                        calls.push(tc);
                    }
                }
            }
        }
        if !calls.is_empty() {
            return Some(calls);
        }
    }

    None
}

fn parse_tool_call_object(value: &serde_json::Value) -> Option<ToolCall> {
    let obj = value.as_object()?;

    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    let name = obj
        .get("name")
        .or_else(|| obj.get("function").and_then(|f| f.get("name")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;

    let arguments = obj
        .get("arguments")
        .or_else(|| obj.get("function").and_then(|f| f.get("arguments")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    Some(ToolCall {
        id,
        r#type: "function".to_string(),
        function: crate::models::FunctionCall { name, arguments },
    })
}

fn unwrap_biz(body: serde_json::Value, status: reqwest::StatusCode) -> Result<serde_json::Value, GatewayError> {
    if status.is_success() {
        if body.get("code").and_then(|v| v.as_i64()) != Some(0) {
            let msg = body
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown DeepSeek API error");
            return Err(GatewayError::Provider(format!("DeepSeek API error: {msg}")));
        }
        if let Some(biz) = body.get("data").and_then(|d| d.get("biz_data")) {
            return Ok(biz.clone());
        }
    }
    Err(GatewayError::Provider(format!(
        "DeepSeek API returned {status}: {body}"
    )))
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_thinking_tracks_model_family() {
        assert!(default_thinking("deepseek-reasoner"));
        assert!(default_thinking("deepseek-expert"));
        assert!(!default_thinking("deepseek-chat"));
        assert!(!default_thinking("deepseek-instant"));
        assert!(!default_thinking("deepseek-vision"));
        assert!(!default_thinking("unknown-model"));
    }

    #[test]
    fn snapshot_routes_think_to_reasoning_and_response_to_content() {
        let line = br#"data: {"v":{"response":{"message_id":123,"fragments":[{"type":"THINK","content":"think text"},{"type":"RESPONSE","content":"final"}]}}}"#;
        let mut counter = 0;
        let mut state = StreamState::new(&[]);
        let (chunks, msg_id) = parse_sse_line(
            line,
            "deepseek-chat",
            "prefix",
            &None,
            false,
            false,
            &mut counter,
            &mut state,
        )
        .unwrap();

        assert_eq!(msg_id, Some(123));
        assert_eq!(chunks.len(), 2);

        let first = &chunks[0];
        assert_eq!(first.choices[0].delta.reasoning_content, Some("think text".to_string()));
        assert_eq!(first.choices[0].delta.content, None);
        assert_eq!(first.choices[0].delta.role, Some("assistant".to_string()));

        let second = &chunks[1];
        assert_eq!(second.choices[0].delta.content, Some("final".to_string()));
        assert_eq!(second.choices[0].delta.reasoning_content, None);
        assert_eq!(second.choices[0].delta.role, None);
    }

    #[test]
    fn path_appends_use_snapshot_kind_map() {
        let snapshot = br#"data: {"v":{"response":{"fragments":[{"type":"THINK","content":"t"},{"type":"RESPONSE","content":"c"}]}}}"#;
        let mut counter = 0;
        let mut state = StreamState::new(&[]);
        let _ = parse_sse_line(snapshot, "m", "p", &None, false, false, &mut counter, &mut state).unwrap();

        // -2 maps to THINK, -1 maps to RESPONSE.
        let reasoning_append = br#"data: {"p":"response/fragments/-2/content","o":"APPEND","v":" extra reasoning"}"#;
        let (chunks, _) = parse_sse_line(reasoning_append, "m", "p", &None, false, false, &mut counter, &mut state).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].choices[0].delta.reasoning_content, Some(" extra reasoning".to_string()));
        assert_eq!(chunks[0].choices[0].delta.content, None);

        let content_append = br#"data: {"p":"response/fragments/-1/content","o":"APPEND","v":" extra content"}"#;
        let (chunks, _) = parse_sse_line(content_append, "m", "p", &None, false, false, &mut counter, &mut state).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].choices[0].delta.content, Some(" extra content".to_string()));
        assert_eq!(chunks[0].choices[0].delta.reasoning_content, None);
    }

    #[test]
    fn message_id_path_append_is_captured() {
        let line = br#"data: {"p":"response/fragments/-1/message_id","o":"APPEND","v":999}"#;
        let mut counter = 0;
        let mut state = StreamState::new(&[]);
        let (chunks, msg_id) = parse_sse_line(line, "m", "p", &None, false, false, &mut counter, &mut state).unwrap();
        assert!(chunks.is_empty());
        assert_eq!(msg_id, Some(999));
    }

    #[test]
    fn capture_message_id_checks_multiple_locations() {
        let mut snapshot = serde_json::Map::new();
        let mut response = serde_json::Map::new();
        response.insert("id".to_string(), serde_json::Value::Number(42.into()));
        snapshot.insert("response".to_string(), serde_json::Value::Object(response));
        assert_eq!(capture_message_id(&snapshot), Some(42));

        let mut snapshot = serde_json::Map::new();
        snapshot.insert("message_id".to_string(), serde_json::Value::Number(77.into()));
        assert_eq!(capture_message_id(&snapshot), Some(77));
    }

    #[test]
    fn citation_extraction_from_snapshot() {
        let snapshot = serde_json::json!({
            "response": {
                "fragments": [{"type": "RESPONSE", "content": "answer"}],
                "search_results": [
                    {"index": 1, "title": "T", "url": "https://x", "snippet": "S"}
                ]
            }
        });
        let map = snapshot.as_object().unwrap().clone();
        let citations = extract_citations(&map).unwrap();
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].index, Some(1));
        assert_eq!(citations[0].title, Some("T".to_string()));
        assert_eq!(citations[0].url, Some("https://x".to_string()));
        assert_eq!(citations[0].snippet, Some("S".to_string()));
    }

    #[test]
    fn effective_model_type_auto_detects_vision() {
        assert_eq!(effective_model_type("deepseek-chat", true), Some("vision"));
        assert_eq!(effective_model_type("deepseek-chat", false), Some("default"));
        assert_eq!(effective_model_type("deepseek-vision", false), Some("vision"));
    }

    #[test]
    fn parse_multiline_tool_call() {
        let content = "<tool_call>\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"main.py\"}}\n</tool_call>";
        let calls = parse_tool_calls_from_content(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"main.py"}"#);
    }

    #[test]
    fn parse_multiple_tool_calls() {
        let content = concat!(
            "<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.py\"}}</tool_call>",
            "<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"b.py\"}}</tool_call>"
        );
        let calls = parse_tool_calls_from_content(content).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.py"}"#);
        assert_eq!(calls[1].function.arguments, r#"{"path":"b.py"}"#);
    }

    #[test]
    fn process_content_text_streams_text_and_collects_tool_call() {
        let mut state = StreamState::new(&[]);
        let text = process_content_text(
            "Hello <tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a\"}}</tool_call> world",
            &mut state,
        );
        assert_eq!(text, "Hello  world");
        assert_eq!(state.collected_tool_calls.len(), 1);
        assert_eq!(state.collected_tool_calls[0].function.name, "read_file");
    }

    #[test]
    fn process_content_text_handles_split_tool_call() {
        let mut state = StreamState::new(&[]);
        let t1 = process_content_text("Hello <tool_call>{\"name\":\"read_", &mut state);
        assert_eq!(t1, "Hello ");
        // The tool call is not closed yet, so no call has been collected.
        assert!(!state.saw_tool_calls);

        let t2 = process_content_text(
            "file\",\"arguments\":{\"path\":\"a\"}}</tool_call> done",
            &mut state,
        );
        assert_eq!(t2, " done");
        assert!(state.saw_tool_calls);
        assert_eq!(state.collected_tool_calls.len(), 1);
        assert_eq!(state.collected_tool_calls[0].function.name, "read_file");
    }

    #[test]
    fn snapshot_streams_text_and_collects_tool_call() {
        let line = br#"data: {"v":{"response":{"message_id":123,"fragments":[{"type":"RESPONSE","content":"I will read <tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"main.py\"}}</tool_call> for you"}]}}}"#;
        let mut counter = 0;
        let mut state = StreamState::new(&[]);
        let (chunks, msg_id) = parse_sse_line(
            line,
            "deepseek-chat",
            "prefix",
            &None,
            false,
            true,
            &mut counter,
            &mut state,
        )
        .unwrap();

        assert_eq!(msg_id, Some(123));
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].choices[0].delta.content,
            Some("I will read  for you".to_string())
        );
        assert_eq!(
            chunks[0].choices[0].delta.role,
            Some("assistant".to_string())
        );
        assert_eq!(state.collected_tool_calls.len(), 1);
        assert_eq!(state.collected_tool_calls[0].function.name, "read_file");
    }

    #[test]
    fn parse_three_parallel_tool_calls() {
        let content = concat!(
            "<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.py\"}}</tool_call>",
            "<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"b.py\"}}</tool_call>",
            "<tool_call>{\"name\":\"grep\",\"arguments\":{\"pattern\":\"foo\"}}</tool_call>"
        );
        let calls = parse_tool_calls_from_content(content).unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.py"}"#);
        assert_eq!(calls[1].function.arguments, r#"{"path":"b.py"}"#);
        assert_eq!(calls[2].function.name, "grep");
        assert_eq!(calls[2].function.arguments, r#"{"pattern":"foo"}"#);
    }
}
