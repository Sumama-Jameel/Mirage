//! GLM/Z.AI direct internal-API client.
//!
//! Bypasses the chat UI and calls `chat.z.ai/api/v2/chat/completions` directly,
//! using the imported browser session only for authentication, signing, and
//! best-effort captcha assistance.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures::stream::{BoxStream, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, ORIGIN, USER_AGENT};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info, warn};

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatMessageDelta, ChunkChoice, Citation, ToolCall, Usage,
};
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::tool_call::{
    convert_xml_tool_calls, format_tool_results, inject_tool_prompt,
};
use crate::session::{SessionHandle, SessionManager};

use super::auth::{resolve_auth, AuthContext};
use super::humanize::CAPTCHA_STEALTH_INIT_JS;
use super::models::GlmModelDef;
use super::rpc::{
    build_completion_body, last_user_text, parse_sse_line, FeatureToggles, X_FE_VERSION,
};
use super::signature::generate_signature;
use super::state::SessionStore;
use super::upload::UploadService;

pub const CHAT_Z_AI_URL: &str = "https://chat.z.ai";
const TIMEOUT_SECS: u64 = 180;

/// Errors from the direct path that are recoverable via UI fallback.
#[derive(Debug)]
pub enum DirectError {
    /// The request cannot be satisfied without driving the browser UI.
    Fallback(String),
    /// A hard error that should be reported to the caller.
    Fatal(GatewayError),
}

impl From<GatewayError> for DirectError {
    fn from(e: GatewayError) -> Self {
        DirectError::Fatal(e)
    }
}

impl std::fmt::Display for DirectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirectError::Fallback(s) => f.write_str(s),
            DirectError::Fatal(e) => write!(f, "{e}"),
        }
    }
}

/// Browser fingerprint values sent as URL query parameters to Z.AI. The
/// upstream backend compares these against the fingerprint recorded for the
/// session at login time, so we must use the real values from the warmed
/// browser rather than static defaults.
#[derive(Debug, Clone)]
struct Fingerprint {
    user_agent: String,
    language: String,
    languages: String,
    timezone: String,
    cookie_enabled: String,
    screen_width: String,
    screen_height: String,
    screen_resolution: String,
    viewport_width: String,
    viewport_height: String,
    viewport_size: String,
    color_depth: String,
    pixel_ratio: String,
    host: String,
    hostname: String,
    protocol: String,
    referrer: String,
    title: String,
    timezone_offset: String,
    local_time: String,
    utc_time: String,
    is_mobile: String,
    is_touch: String,
    max_touch_points: String,
    browser_name: String,
    os_name: String,
}

impl Fingerprint {
    /// Fallback values when the page fingerprint cannot be extracted. The
    /// warmed session's user agent is used; everything else matches the
    /// current Chrome-on-Windows profile the gateway is most commonly
    /// deployed with. The direct path will fail upstream validation if the
    /// real fingerprint is required, triggering UI fallback.
    fn defaults(user_agent: &str) -> Self {
        Self {
            user_agent: user_agent.to_string(),
            language: "zh-CN".into(),
            languages: "zh-CN,zh".into(),
            timezone: "Asia/Shanghai".into(),
            cookie_enabled: "true".into(),
            screen_width: "1920".into(),
            screen_height: "1080".into(),
            screen_resolution: "1920x1080".into(),
            viewport_width: "1920".into(),
            viewport_height: "969".into(),
            viewport_size: "1920x969".into(),
            color_depth: "24".into(),
            pixel_ratio: "1".into(),
            host: "chat.z.ai".into(),
            hostname: "chat.z.ai".into(),
            protocol: "https".into(),
            referrer: String::new(),
            title: "Z.ai".into(),
            timezone_offset: "-480".into(),
            local_time: String::new(),
            utc_time: String::new(),
            is_mobile: "false".into(),
            is_touch: "false".into(),
            max_touch_points: "0".into(),
            browser_name: if user_agent.contains("Firefox") || user_agent.contains("Gecko") {
                "Firefox"
            } else {
                "Chrome"
            }
            .into(),
            os_name: if user_agent.contains("Linux") {
                "Linux"
            } else if user_agent.contains("Mac") {
                "MacIntel"
            } else {
                "Windows"
            }
            .into(),
        }
    }
}

impl Default for Fingerprint {
    fn default() -> Self {
        Self {
            user_agent: String::new(),
            language: String::new(),
            languages: String::new(),
            timezone: String::new(),
            cookie_enabled: String::new(),
            screen_width: String::new(),
            screen_height: String::new(),
            screen_resolution: String::new(),
            viewport_width: String::new(),
            viewport_height: String::new(),
            viewport_size: String::new(),
            color_depth: String::new(),
            pixel_ratio: String::new(),
            host: String::new(),
            hostname: String::new(),
            protocol: String::new(),
            referrer: String::new(),
            title: String::new(),
            timezone_offset: String::new(),
            local_time: String::new(),
            utc_time: String::new(),
            is_mobile: String::new(),
            is_touch: String::new(),
            max_touch_points: String::new(),
            browser_name: String::new(),
            os_name: String::new(),
        }
    }
}

/// Direct API client for Z.AI's internal chat-completions endpoint.
pub struct GlmDirectClient {
    http: reqwest::Client,
    auth: AuthContext,
    store: SessionStore,
    model: GlmModelDef,
    sign_secret: String,
    upstream_url: String,
    sessions: SessionManager,
    session_id: String,
    user_agent: String,
    cookie_jar: std::sync::Arc<obscura_net::CookieJar>,
    models_cache: std::sync::Mutex<Option<serde_json::Value>>,
}

impl GlmDirectClient {
    /// Create a client for one request, resolving auth from the imported
    /// browser session.
    pub async fn new(
        sessions: &SessionManager,
        session: &SessionHandle,
        store: SessionStore,
        model: &GlmModelDef,
        sign_secret: &str,
        upstream_url: &str,
    ) -> Result<Self, DirectError> {
        let auth = resolve_auth(&session.local_storage, &session.cookie_jar)
            .await
            .map_err(|e| DirectError::Fallback(e.to_string()))?;

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("Accept", HeaderValue::from_static("text/event-stream, application/json"));
        headers.insert("X-FE-Version", HeaderValue::from_static(X_FE_VERSION));
        headers.insert(ORIGIN, HeaderValue::from_static(CHAT_Z_AI_URL));
        headers.insert("sec-fetch-dest", HeaderValue::from_static("empty"));
        headers.insert("sec-fetch-mode", HeaderValue::from_static("cors"));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        headers.insert("Accept-Language", HeaderValue::from_static("zh-CN,zh;q=0.9"));

        // Use the real warmed session's User-Agent and keep the client hints
        // consistent with it. Sending a hardcoded Chrome UA together with
        // Chrome `sec-ch-ua` hints while the session is Firefox (or vice
        // versa) is what triggers Z.AI's "请刷新页面以更新应用后重试" rejection.
        let session_ua = &session.user_agent;
        let is_firefox = session_ua.contains("Firefox") || session_ua.contains("Gecko");
        let ua_value = HeaderValue::from_str(session_ua)
            .unwrap_or_else(|_| HeaderValue::from_static("Mozilla/5.0"));
        headers.insert(USER_AGENT, ua_value);
        if is_firefox {
            // Firefox does not send Chromium client hints. Leave them off so
            // the fingerprint stays self-consistent.
            headers.remove("sec-ch-ua");
            headers.remove("sec-ch-ua-mobile");
            headers.remove("sec-ch-ua-platform");
        } else {
            headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
            headers.insert(
                "sec-ch-ua",
                HeaderValue::from_static(
                    r#""Chromium";v="140", "Not=A?Brand";v="24", "Google Chrome";v="140""#,
                ),
            );
            headers.insert("sec-ch-ua-platform", HeaderValue::from_static("\"Windows\""));
        }

        // Note: the `cookie` header is NOT set here. Z.AI sets anti-bot
        // cookies (notably `ssxmod_itna`) via client-side JavaScript on the
        // chat.z.ai page. We must navigate the warmed browser first, wait
        // for the SPA to load, and then read the live cookie jar before
        // sending the direct API request. See `chat_stream`.

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .default_headers(headers)
            .build()
            .map_err(|e| GatewayError::Internal(format!("GLM HTTP client failed: {e}")))?;

        info!(
            session_id = %session.id,
            model = %model.id,
            "GLM DirectClient initialized"
        );

        Ok(Self {
            http,
            auth,
            store,
            model: model.clone(),
            sign_secret: sign_secret.to_string(),
            upstream_url: upstream_url.to_string(),
            sessions: sessions.clone(),
            session_id: session.id.clone(),
            user_agent: session.user_agent.clone(),
            cookie_jar: session.cookie_jar.clone(),
            models_cache: std::sync::Mutex::new(None),
        })
    }

    /// Warm the browser to the target chat URL and probe `/api/models` to
    /// validate the session and resolve the requested public model id to the
    /// upstream internal id. The model list is cached for the lifetime of this
    /// client (one gateway request).
    async fn resolve_internal_model_id(
        &self,
        chat_id: &str,
    ) -> Result<String, DirectError> {
        {
            let guard = self.models_cache.lock().map_err(|e| {
                DirectError::Fatal(GatewayError::Internal(format!("GLM model cache poisoned: {e}")))
            })?;
            if let Some(cache) = guard.as_ref() {
                if let Some(id) = find_internal_id(cache, &self.model.id) {
                    return Ok(id);
                }
            }
        }

        // Note: we skip SPA navigation here. The cookies were already imported
        // from the user's Firefox session and the auth token was resolved from
        // localStorage. The chat.z.ai SPA fails to load in Obscura's V8 (Svelte 5
        // ES module incompatibility), so we bypass it entirely and use the
        // imported session state directly. Anti-bot cookies (ssxmod_itna etc.)
        // are already present in the imported cookie jar.

        let probe_url = format!("{}/api/models", CHAT_Z_AI_URL);
        let resp = self
            .http
            .get(&probe_url)
            .header(AUTHORIZATION, format!("Bearer {}", self.auth.token))
            .send()
            .await
            .map_err(|e| DirectError::Fallback(format!("GLM model probe request failed: {e}")))?;

        let status = resp.status();
        let status_u16 = status.as_u16();
        if status_u16 == 401 || status_u16 == 403 {
            return Err(DirectError::Fallback(format!(
                "GLM model probe returned {status}; session invalid or captcha required"
            )));
        }
        if status_u16 == 400 || status_u16 == 429 || status_u16 == 426 {
            let text = resp.text().await.unwrap_or_default();
            if is_fallback_error_text(&text) {
                return Err(DirectError::Fallback(format!(
                    "GLM model probe returned {status}: {text}"
                )));
            }
            return Err(DirectError::Fatal(GatewayError::Provider(format!(
                "GLM model probe returned {status}: {}",
                text.chars().take(500).collect::<String>()
            ))));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(DirectError::Fatal(GatewayError::Provider(format!(
                "GLM model probe returned {status}: {}",
                text.chars().take(500).collect::<String>()
            ))));
        }

        let body = resp.json::<serde_json::Value>().await.map_err(|e| {
            DirectError::Fatal(GatewayError::Provider(format!(
                "GLM model probe decode failed: {e}"
            )))
        })?;

        if let Some(err) = extract_app_error(&body) {
            return Err(DirectError::Fallback(format!(
                "GLM model probe returned application error: {err}"
            )));
        }

        let resolved = find_internal_id(&body, &self.model.id)
            .unwrap_or_else(|| self.model.internal_id.clone());

        {
            let mut guard = self.models_cache.lock().map_err(|e| {
                DirectError::Fatal(GatewayError::Internal(format!("GLM model cache poisoned: {e}")))
            })?;
            *guard = Some(body);
        }

        Ok(resolved)
    }

    /// Non-streaming chat completion.
    pub async fn chat(self, request: ChatCompletionRequest) -> Result<ChatCompletionResponse, DirectError> {
        let model_id = request.model.clone();
        let had_tools = request.tools.is_some();
        let store = self.store.clone();

        let mut stream = self.chat_stream(request.clone()).await?;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut citations: Vec<Citation> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut session_url: Option<String> = None;

        while let Some(result) = stream.next().await {
            let chunk = result.map_err(|e| DirectError::Fallback(e.to_string()))?;
            if session_url.is_none() && chunk.session_url.is_some() {
                session_url = chunk.session_url.clone();
            }
            for choice in &chunk.choices {
                if let Some(ref c) = choice.delta.content {
                    text.push_str(c);
                }
                if let Some(ref r) = choice.delta.reasoning_content {
                    reasoning.push_str(r);
                }
                if let Some(ref c) = choice.delta.citations {
                    for citation in c {
                        if !citations.iter().any(|existing| existing.url == citation.url && existing.index == citation.index) {
                            citations.push(citation.clone());
                        }
                    }
                }
                if let Some(ref calls) = choice.delta.tool_calls {
                    for call in calls {
                        if !tool_calls.iter().any(|existing| existing.id == call.id) {
                            tool_calls.push(call.clone());
                        }
                    }
                }
            }
        }

        if had_tools {
            let (cleaned_text, parsed) = convert_xml_tool_calls(&text, true);
            if let Some(calls) = parsed {
                if !calls.is_empty() {
                    tool_calls = calls;
                    text = cleaned_text;
                }
            }
        }

        let has_tool_calls = !tool_calls.is_empty();

        if let Some(ref url) = session_url {
            if let Some(chat_id) = extract_chat_id(url) {
                store.insert(chat_id.clone(), &model_id).await;
                if has_tool_calls {
                    store.store_tool_calls(&chat_id, &tool_calls).await;
                }
            }
        }

        let prompt_text: String = request.messages.iter().map(|m| m.content.as_text()).collect();
        let completion_text = format!("{}{}", text, reasoning);
        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: model_id.clone(),
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: crate::models::ChatContent::String(text),
                    name: None,
                    reasoning_content: if reasoning.is_empty() { None } else { Some(reasoning) },
                    citations: if citations.is_empty() { None } else { Some(citations) },
                    tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
                    tool_call_id: None,
                },
                finish_reason: if has_tool_calls { "tool_calls".to_string() } else { "stop".to_string() },
            }],
            usage: Usage {
                prompt_tokens: estimate_tokens("glm", &model_id, &prompt_text),
                completion_tokens: estimate_tokens("glm", &model_id, &completion_text),
                total_tokens: estimate_tokens("glm", &model_id, &prompt_text)
                    + estimate_tokens("glm", &model_id, &completion_text),
            },
            session_url,
        })
    }

    /// Streaming chat completion.
    pub async fn chat_stream(
        self,
        mut request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, DirectError> {
        let model_id = request.model.clone();
        // GLM's web client (chat.z.ai) keys a conversation by a single
        // linear chat_id; the session_url carries it directly and there is
        // no message-tree or fork-from-a-prior-message flow in its API
        // surface. Continuations serialize on this one thread, matching the
        // web app. No lock is held; chat_id itself is the continuity handle.
        let chat_id = self.resolve_or_create_chat(&request.session_url).await?;
        let session_url = Some(format!("{}/c/{}", CHAT_Z_AI_URL, chat_id));

        // Warm the browser and validate the session against the live model list.
        // This resolves the public model id to the upstream internal id and
        // catches captcha/auth challenges early so we can fall back to UI.
        let resolved_internal_model_id = self.resolve_internal_model_id(&chat_id).await?;

        // Handle tool-result messages by looking up prior calls stored for this
        // chat and formatting them into the last user message.
        let tool_context = self.handle_tool_results(&chat_id, &request.messages).await;
        if !tool_context.is_empty() {
            inject_tool_context(&mut request.messages, &tool_context);
        }

        // Inject tool definitions into the last user message when tools are
        // requested. GLM's internal endpoint does not natively support OpenAI
        // tool schemas, so the model emits <tool_call> markers instead.
        if let Some(ref tools) = request.tools {
            let last_prompt = last_user_text(&request.messages);
            let injected = inject_tool_prompt(&last_prompt, tools, request.tool_choice.as_ref());
            update_last_user_text(&mut request.messages, &injected);
        }

        // Upload any attachments in the last user message. On failure, fall back
        // to UI automation which can drive the native file picker.
        let upload_service = UploadService::new(
            self.http.clone(),
            self.auth.token.clone(),
            chat_id.clone(),
        );
        let files = match upload_service.prepare_attachments(&mut request.messages).await {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "GLM direct attachment upload failed; falling back to UI");
                return Err(DirectError::Fallback(e.to_string()));
            }
        };

        let has_attachments = !files.is_empty();
        let last_user = last_user_text(&request.messages);
        let signature = generate_signature(&self.auth.user_id, &last_user, &self.sign_secret)
            .map_err(|e| GatewayError::Internal(format!("GLM signature failed: {e}")))?;

        let request_id = signature.request_id.clone();
        let body = build_completion_body(
            &request,
            &self.model,
            &chat_id,
            &request_id,
            &last_user,
            &files,
            FeatureToggles {
                thinking: request.thinking.unwrap_or(false),
                search: request.search.unwrap_or(false),
            },
            &resolved_internal_model_id,
        );

        debug!(body = %serde_json::to_string(&body).unwrap_or_default(), "GLM direct request body");

        // Extract the real browser fingerprint from the warmed page. Z.AI
        // compares these values against the fingerprint recorded for the
        // session at login, so hardcoded defaults cause the upstream to
        // reject with "请刷新页面以更新应用后重试".
        let fingerprint = match self.extract_fingerprint().await {
            Ok(fp) => fp,
            Err(e) => {
                warn!(error = %e, "GLM fingerprint extract failed; using session user agent fallback");
                Fingerprint::defaults(&self.user_agent)
            }
        };

        let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let counter = AtomicU32::new(0);
        let store = self.store.clone();
        let model_id_for_stream = model_id.clone();
        let session_url_for_stream = session_url.clone();
        let chat_id_for_store = chat_id.clone();
        let had_tools = request.tools.is_some();

        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

        let mut captcha_param: Option<serde_json::Value> = None;

        let stream_result: Result<
            BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
            DirectError,
        > = async {
            // Up to two attempts: first attempt may return
            // FRONTEND_CAPTCHA_REQUIRED; on the second attempt we attach the
            // solved `captcha_verify_param` to the body.
            for attempt in 1u8..=2 {
                // Regenerate the signature on every attempt (the SPA does
                // the same on retry).
                let signature = generate_signature(&self.auth.user_id, &last_user, &self.sign_secret)
                    .map_err(|e| DirectError::Fatal(GatewayError::Internal(format!("GLM signature failed: {e}"))))?;
                let request_id = signature.request_id.clone();
                let mut body = build_completion_body(
                    &request,
                    &self.model,
                    &chat_id,
                    &request_id,
                    &last_user,
                    &files,
                    FeatureToggles {
                        thinking: request.thinking.unwrap_or(false),
                        search: request.search.unwrap_or(false),
                    },
                    &resolved_internal_model_id,
                );
                if let Some(param) = &captcha_param {
                    body["captcha_verify_param"] = param.clone();
                } else {
                    body["captcha_verify_param"] = serde_json::Value::Null;
                }

                let url = build_upstream_url(
                    &self.upstream_url,
                    &self.auth,
                    &signature,
                    &chat_id,
                    &fingerprint,
                )?;

                // Make the POST request from INSIDE the Obscura page via
                // globalThis.fetch(). The request goes through op_fetch_url
                // using the page's own TCP/TLS/cookies/IP — the backend sees a
                // request indistinguishable from the real SPA. We capture the
                // response body via the on_response Rust callback (lock-free).

                // Navigate to chat.z.ai first so the page's origin/referer
                // match (same-origin request). Even though the SPA can't fully
                // load, the document URL will be set correctly.
                self.sessions.navigate(&self.session_id, CHAT_Z_AI_URL).await?;
                self.sessions.pump_event_loop(&self.session_id, 2000).await?;

                let body_json_str = serde_json::to_string(&body)
                    .map_err(|e| DirectError::Fatal(GatewayError::Internal(format!("JSON serialization failed: {e}"))))?;

                let headers_json = serde_json::json!({
                    "Content-Type": "application/json",
                    "Accept": "text/event-stream, application/json",
                    "X-FE-Version": X_FE_VERSION,
                    "X-Signature": signature.value,
                    "Authorization": format!("Bearer {}", self.auth.token),
                    "Accept-Language": "zh-CN,zh;q=0.9",
                });
                let headers_json_str = serde_json::to_string(&headers_json)
                    .map_err(|e| DirectError::Fatal(GatewayError::Internal(format!("headers serialization failed: {e}"))))?;

                // Kick off the async fetch. We don't wait for the Promise;
                // the on_response callback will store the result.
                let kick_js = format!(
                    r#"(function() {{
                        fetch({}, {{
                            method: "POST",
                            headers: JSON.parse({}),
                            body: {}
                        }});
                        return "kicked";
                    }})()"#,
                    serde_json::Value::String(url),
                    serde_json::Value::String(headers_json_str),
                    serde_json::Value::String(body_json_str),
                );

                let _kick = self
                    .sessions
                    .execute_js(&self.session_id, &kick_js)
                    .await
                    .map_err(|e| DirectError::Fallback(format!("in-page fetch kick failed: {e}")))?;

                // Pump the Deno event loop and poll for the response.
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                let body_text: String;
                let status: u16;
                let captured_headers: std::collections::HashMap<String, String>;
                loop {
                    if std::time::Instant::now() > deadline {
                        return Err(DirectError::Fallback("in-page fetch timed out".to_string()));
                    }
                    // Pump the event loop to let op_fetch_url's future progress
                    self.sessions.pump_event_loop(&self.session_id, 500).await
                        .map_err(|e| DirectError::Fallback(format!("pump failed: {e}")))?;

                    // Check if the on_response callback captured our response
                    match self.sessions.get_captcha_response_body(&self.session_id).await {
                        Ok(Some(body)) => {
                            // Also capture was captured alongside — it's
                            // always the last /api/v2/chat/completions response.
                            body_text = String::from_utf8(body)
                                .map_err(|_| DirectError::Fallback("captured body not utf-8".to_string()))?;
                            status = 200;
                            captured_headers = std::collections::HashMap::new();
                            break;
                        }
                        Ok(None) => {
                            // Also check JS outcome as fallback
                            let poll_js = "(function(){var s=window.__glm_resp_status;if(s===undefined)return null;return JSON.stringify({status:s,body:window.__glm_resp_body||''});})()";
                            let poll_result = self
                                .sessions
                                .execute_js(&self.session_id, poll_js)
                                .await
                                .ok();
                            if let Some(serde_json::Value::String(ref s)) = poll_result {
                                if let Ok(data) = serde_json::from_str::<serde_json::Value>(s) {
                                    if let Some(st) = data.get("status").and_then(|v| v.as_i64()) {
                                        if st != 0 {
                                            status = st as u16;
                                            body_text = data.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            captured_headers = std::collections::HashMap::new();
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => return Err(DirectError::Fallback(format!("capture read failed: {e}"))),
                    }
                }

                let body_bytes = body_text.into_bytes();
                let (first_event, remaining_bytes) = split_first_sse_event(&body_bytes);

                if attempt == 1 {
                    if let Some(code) = first_event
                        .as_ref()
                        .and_then(|e| e.error_code.as_deref())
                    {
                        if code == "FRONTEND_CAPTCHA_REQUIRED" {
                            info!(chat_id = %chat_id, "GLM requires captcha; solving in page");
                            match self.solve_captcha().await {
                                Ok(param) => {
                                    info!(
                                        chat_id = %chat_id,
                                        param_kind = match &param {
                                            serde_json::Value::String(s) => format!("string(len={})", s.len()),
                                            serde_json::Value::Object(_) => "object".to_string(),
                                            serde_json::Value::Null => "null".to_string(),
                                            _ => "other".to_string(),
                                        },
                                        param_preview = %param.to_string().chars().take(120).collect::<String>(),
                                        "GLM captcha param obtained"
                                    );
                                    captcha_param = Some(param);
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    }
                }

                // No captcha retry needed (or already retried): spawn the
                // streaming task over the buffered bytes.
                {
                    let preview_len = body_bytes.len().min(800);
                    let preview = String::from_utf8_lossy(&body_bytes[..preview_len]);
                    let fe = &first_event;
                    info!(
                        body_len = body_bytes.len(),
                        first_event_error = fe.as_ref().and_then(|e| e.error.clone()).unwrap_or_default(),
                        first_event_error_code = fe.as_ref().and_then(|e| e.error_code.clone()).unwrap_or_default(),
                        first_event_has_content = fe.as_ref().and_then(|e| e.content_delta.as_ref()).is_some(),
                        first_event_has_reasoning = fe.as_ref().and_then(|e| e.reasoning_delta.as_ref()).is_some(),
                        first_event_done = fe.as_ref().map(|e| e.done).unwrap_or(false),
                        preview = %preview,
                        "GLM streaming response body preview"
                    );
                }
                // If the upstream still returns an error event after the
                // captcha retry, surface it instead of silently streaming
                // an empty body.
                if let Some(ev) = &first_event {
                    if let Some(err) = &ev.error {
                        return Err(DirectError::Fallback(format!(
                            "GLM upstream error: {err}"
                        )));
                    }
                }
                let tx_clone = tx.clone();
                let id_prefix_clone = id_prefix.clone();
                tokio::spawn(async move {
                    let mut emitted_role = false;
                    let mut collected_text = String::new();
                    let mut collected_tool_calls: Vec<ToolCall> = Vec::new();

                    // Emit any content carried by the peeked first event so
                    // the caller doesn't miss the start of the stream.
                    if let Some(ev) = first_event {
                        if let Some(delta) = ev.content_delta {
                            collected_text.push_str(&delta);
                            if send_content_chunk(
                                &tx_clone,
                                &counter,
                                &id_prefix_clone,
                                &model_id_for_stream,
                                &delta,
                                &session_url_for_stream,
                                &mut emitted_role,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                        if let Some(delta) = ev.edit_delta {
                            collected_text.push_str(&delta);
                            if send_content_chunk(
                                &tx_clone,
                                &counter,
                                &id_prefix_clone,
                                &model_id_for_stream,
                                &delta,
                                &session_url_for_stream,
                                &mut emitted_role,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                        if let Some(delta) = ev.reasoning_delta {
                            if send_reasoning_chunk(
                                &tx_clone,
                                &counter,
                                &id_prefix_clone,
                                &model_id_for_stream,
                                &delta,
                                &session_url_for_stream,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                        if let Some(citations) = ev.citations {
                            counter.fetch_add(1, Ordering::Relaxed);
                            let _ = tx_clone.send(Ok(ChatCompletionChunk {
                                id: format!(
                                    "{}-{}",
                                    id_prefix_clone,
                                    counter.load(Ordering::Relaxed)
                                ),
                                object: "chat.completion.chunk".to_string(),
                                created: current_timestamp(),
                                model: model_id_for_stream.clone(),
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
                                session_url: session_url_for_stream.clone(),
                            }));
                        }
                    }

                    // Stream the remaining bytes through the same
                    // line-buffering SSE loop.
                    let chunk_size = 8 * 1024usize;
                    let chunks: Vec<Result<Vec<u8>, GatewayError>> = if remaining_bytes.is_empty()
                    {
                        Vec::new()
                    } else {
                        remaining_bytes
                            .chunks(chunk_size)
                            .map(|c| Ok(c.to_vec()))
                            .collect()
                    };
                    let mut stream = futures::stream::iter(chunks);
                    let mut buffer: Vec<u8> = Vec::new();
                    while let Some(chunk_result) = stream.next().await {
                        let bytes = match chunk_result {
                            Ok(b) => b,
                            Err(_) => return,
                        };
                        buffer.extend_from_slice(&bytes);
                        let mut consumed = 0;
                        for (i, window) in buffer.windows(2).enumerate() {
                            if window == b"\n\n" || window == b"\r\n" {
                                let line = &buffer[consumed..i];
                                if let Some(event) = parse_sse_line(
                                    std::str::from_utf8(line).unwrap_or(""),
                                ) {
                                    if event.error.is_some() {
                                        let _ = tx_clone.send(Err(GatewayError::Provider(format!(
                                            "GLM upstream error: {}",
                                            event.error.unwrap_or_default()
                                        ))));
                                        return;
                                    }
                                    if let Some(delta) = event.content_delta {
                                        collected_text.push_str(&delta);
                                        if send_content_chunk(
                                            &tx_clone,
                                            &counter,
                                            &id_prefix_clone,
                                            &model_id_for_stream,
                                            &delta,
                                            &session_url_for_stream,
                                            &mut emitted_role,
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    if let Some(delta) = event.edit_delta {
                                        collected_text.push_str(&delta);
                                        if send_content_chunk(
                                            &tx_clone,
                                            &counter,
                                            &id_prefix_clone,
                                            &model_id_for_stream,
                                            &delta,
                                            &session_url_for_stream,
                                            &mut emitted_role,
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    if let Some(delta) = event.reasoning_delta {
                                        if send_reasoning_chunk(
                                            &tx_clone,
                                            &counter,
                                            &id_prefix_clone,
                                            &model_id_for_stream,
                                            &delta,
                                            &session_url_for_stream,
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    if let Some(citations) = event.citations {
                                        counter.fetch_add(1, Ordering::Relaxed);
                                        let _ = tx_clone.send(Ok(ChatCompletionChunk {
                                            id: format!(
                                                "{}-{}",
                                                id_prefix_clone,
                                                counter.load(Ordering::Relaxed)
                                            ),
                                            object: "chat.completion.chunk".to_string(),
                                            created: current_timestamp(),
                                            model: model_id_for_stream.clone(),
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
                                            session_url: session_url_for_stream.clone(),
                                        }));
                                    }
                                    if event.done {
                                        break;
                                    }
                                }
                                consumed = i + 2;
                            }
                        }
                        buffer.drain(0..consumed);
                    }

                    // Post-stream tool-call extraction.
                    let mut finish_reason = "stop".to_string();
                    if had_tools && !collected_text.is_empty() {
                        let (_cleaned, parsed) = convert_xml_tool_calls(&collected_text, true);
                        if let Some(calls) = parsed {
                            if !calls.is_empty() {
                                counter.fetch_add(1, Ordering::Relaxed);
                                let _ = tx_clone.send(Ok(ChatCompletionChunk {
                                    id: format!(
                                        "{}-{}",
                                        id_prefix_clone,
                                        counter.load(Ordering::Relaxed)
                                    ),
                                    object: "chat.completion.chunk".to_string(),
                                    created: current_timestamp(),
                                    model: model_id_for_stream.clone(),
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
                                    session_url: session_url_for_stream.clone(),
                                }));
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
                        store
                            .insert(chat_id_for_store.clone(), &model_id_for_stream)
                            .await;
                        store
                            .store_tool_calls(&chat_id_for_store, &collected_tool_calls)
                            .await;
                    }

                    counter.fetch_add(1, Ordering::Relaxed);
                    let _ = tx_clone.send(Ok(ChatCompletionChunk {
                        id: format!("{}-final", id_prefix_clone),
                        object: "chat.completion.chunk".to_string(),
                        created: current_timestamp(),
                        model: model_id_for_stream,
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: ChatMessageDelta::default(),
                            finish_reason: Some(finish_reason),
                        }],
                        session_url: session_url_for_stream,
                    }));
                });

                return Ok(UnboundedReceiverStream::new(rx).boxed());
            }
            Err(DirectError::Fallback(
                "GLM direct path exhausted captcha retries".to_string(),
            ))
        }
        .await;

        stream_result
    }

    /// Wait for the chat.z.ai SPA to finish loading. Polls `document.readyState`
    /// and the presence of the `#app` mount node, then falls back to a
    /// fixed delay so anti-bot cookies have time to be set even when the
    /// V8 watchdog kills the initial script execution.
    async fn wait_for_spa_ready(&self) {
        let js = r##"
        (function() {
            try {
                return {
                    ready: document.readyState,
                    has_app: !!document.getElementById('app'),
                    has_input: !!document.querySelector('#chat-input, textarea, [contenteditable="true"]'),
                };
            } catch (e) { return { error: String(e) }; }
        })()
        "##;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if let Ok(value) = self.sessions.execute_js(&self.session_id, js).await {
                if let Some(ready) = value.get("ready").and_then(|v| v.as_str()) {
                    let has_app = value
                        .get("has_app")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let has_input = value
                        .get("has_input")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if (ready == "complete" || ready == "interactive") && (has_app || has_input) {
                        // Give the SPA a moment to finish post-load JS that
                        // sets anti-bot cookies.
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        // Fallback: if `execute_js` keeps failing (watchdog / isolate killed),
        // wait a fixed amount so any cookies the SPA sets have time to land.
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    }

    /// Extract the browser fingerprint from the warmed page. Runs a tiny JS
    /// snippet that reads `navigator`, `screen`, `Intl`, and `document`
    /// properties. The upstream backend compares these against the
    /// fingerprint recorded at login, so we must use the real session values.
    async fn extract_fingerprint(&self) -> Result<Fingerprint, DirectError> {
        let js = r##"
        (function() {
            try {
                var d = new Date();
                var pad = function(n) { return String(n).padStart(2, '0'); };
                var ua = navigator.userAgent || '';
                var isFirefox = /Firefox|Gecko/.test(ua);
                var os = 'Win32';
                if (/Linux/.test(ua)) os = 'Linux';
                else if (/Mac OS X/.test(ua)) os = 'MacIntel';
                else if (/Windows/.test(ua)) os = 'Win32';
                return {
                    user_agent: ua,
                    language: navigator.language || 'en-US',
                    languages: (navigator.languages || []).join(',') || '',
                    timezone: (Intl.DateTimeFormat().resolvedOptions().timeZone) || 'Asia/Shanghai',
                    cookie_enabled: String(!!navigator.cookieEnabled),
                    screen_width: String(window.screen.width),
                    screen_height: String(window.screen.height),
                    screen_resolution: window.screen.width + 'x' + window.screen.height,
                    viewport_width: String(window.innerWidth),
                    viewport_height: String(window.innerHeight),
                    viewport_size: window.innerWidth + 'x' + window.innerHeight,
                    color_depth: String(window.screen.colorDepth || 24),
                    pixel_ratio: String(window.devicePixelRatio || 1),
                    host: 'chat.z.ai',
                    hostname: 'chat.z.ai',
                    protocol: 'https',
                    referrer: '',
                    title: 'Z.ai',
                    timezone_offset: String(d.getTimezoneOffset()),
                    local_time: d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()) + ' ' + pad(d.getHours()) + ':' + pad(d.getMinutes()) + ':' + pad(d.getSeconds()),
                    utc_time: d.toISOString().replace('T', ' ').slice(0, 19),
                    is_mobile: String(/Mobi|Android|iPhone|iPad/.test(ua)),
                    is_touch: String('ontouchstart' in window),
                    max_touch_points: String(navigator.maxTouchPoints || 0),
                    browser_name: isFirefox ? 'Firefox' : 'Chrome',
                    os_name: os
                };
            } catch (e) {
                return { error: String(e) };
            }
        })()
        "##;
        let value = self
            .sessions
            .execute_js(&self.session_id, js)
            .await
            .map_err(|e| {
                DirectError::Fallback(format!("GLM fingerprint extract failed: {e}"))
            })?;
        if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
            return Err(DirectError::Fallback(format!(
                "GLM fingerprint extract error: {err}"
            )));
        }
        let obj = value.as_object().ok_or_else(|| {
            DirectError::Fallback("GLM fingerprint extract returned non-object".to_string())
        })?;
        let s = |k: &str| -> String {
            obj.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Ok(Fingerprint {
            user_agent: s("user_agent"),
            language: s("language"),
            languages: s("languages"),
            timezone: s("timezone"),
            cookie_enabled: s("cookie_enabled"),
            screen_width: s("screen_width"),
            screen_height: s("screen_height"),
            screen_resolution: s("screen_resolution"),
            viewport_width: s("viewport_width"),
            viewport_height: s("viewport_height"),
            viewport_size: s("viewport_size"),
            color_depth: s("color_depth"),
            pixel_ratio: s("pixel_ratio"),
            host: s("host"),
            hostname: s("hostname"),
            protocol: s("protocol"),
            referrer: s("referrer"),
            title: s("title"),
            timezone_offset: s("timezone_offset"),
            local_time: s("local_time"),
            utc_time: s("utc_time"),
            is_mobile: s("is_mobile"),
            is_touch: s("is_touch"),
            max_touch_points: s("max_touch_points"),
            browser_name: s("browser_name"),
            os_name: s("os_name"),
        })
    }

    /// Resolve an existing chat from `session_url` or create a new chat id.
    async fn resolve_or_create_chat(
        &self,
        session_url: &Option<String>,
    ) -> Result<String, DirectError> {
        if let Some(url) = session_url.as_deref() {
            let chat_id = extract_chat_id(url).ok_or_else(|| {
                DirectError::Fatal(GatewayError::BadRequest(format!("invalid GLM session_url: {url}")))
            })?;
            let _ = self
                .store
                .acquire(&chat_id)
                .await
                .ok_or_else(|| {
                    DirectError::Fatal(GatewayError::BadRequest(format!(
                        "expired GLM session_url: {url}"
                    )))
                })?;
            SessionStore::ensure_model_matches(&self.model.id, &self.store.get_model(&chat_id).await.unwrap_or_default())?;
            return Ok(chat_id);
        }

        Ok(uuid::Uuid::new_v4().to_string())
    }

    /// Solve the Aliyun NVC captcha inside the warmed page and return the
    /// `captcha_verify_param` the upstream expects in the chat body.
    ///
    /// Unlike the full SDK flow that requires slider interaction, this extracts
    /// the `SessionId` from the InitCaptchaV3 REST response (which does not
    /// require human interaction) and constructs a minimal captcha token.
    async fn solve_captcha(&self) -> Result<serde_json::Value, DirectError> {
        // Inject stealth init to mask automation
        let stealth_js = format!(
            r#"(function() {{
                try {{
                    {stealth_js}
                    return {{ ok: true }};
                }} catch (e) {{
                    return {{ ok: false, error: String(e) }};
                }}
            }})()"#,
            stealth_js = CAPTCHA_STEALTH_INIT_JS
        );
        let _ = self.sessions.execute_js(&self.session_id, &stealth_js).await;

        // Fetch and execute the Aliyun captcha SDK.
        let sdk_code = fetch_captcha_sdk().await.map_err(|e| {
            DirectError::Fallback(format!("failed to fetch Aliyun captcha SDK: {e}"))
        })?;
        let _ = self.sessions.execute_js(&self.session_id, &sdk_code).await;

        // Read config from the SDK's exposed objects.
        let rt_info = self
            .sessions
            .execute_js(
                &self.session_id,
                r##"(function() {
                    var rt = window.__ALIYUN_RT;
                    var et = window.__ALIYUN_ET;
                    var appKey = rt && rt.appKey && rt.appKey['3.0'];
                    var appKey2 = rt && rt.appKey && rt.appKey['2.0'];
                    var key = appKey || appKey2 || '';
                    var endpoints = et && et['3.0'] || et['2.0'] || [];
                    var cn = endpoints && endpoints['cn'] || [];
                    var host = cn[0] || 'a.captcha-open.aliyuncs.com';
                    return JSON.stringify({ appKey: typeof key === 'object' ? JSON.stringify(key) : String(key), host: host });
                })()"##,
            )
            .await;

        // Initialize captcha inside the page (creates the DOM elements, makes
        // the XHR to InitCaptchaV3).
        let init_js = r##"
            (function() {
                if (!window.initAliyunCaptcha) {
                    return { error: 'initAliyunCaptcha not defined' };
                }
                try {
                    function ensureElement(id) {
                        var el = document.getElementById(id);
                        if (el) return el;
                        el = document.createElement('div');
                        el.id = id;
                        el.style.cssText = 'position:fixed;left:-9999px;top:-9999px;width:320px;height:40px;z-index:2147483647;pointer-events:auto;';
                        document.body.appendChild(el);
                        return el;
                    }
                    function ensureTrigger() {
                        var btn = document.getElementById('chat-captcha-trigger');
                        if (btn) return btn;
                        btn = document.createElement('button');
                        btn.id = 'chat-captcha-trigger';
                        btn.type = 'button';
                        btn.style.cssText = 'position:fixed;left:-9999px;top:-9999px;width:320px;height:40px;z-index:2147483647;opacity:0;pointer-events:auto;';
                        btn.setAttribute('aria-hidden', 'true');
                        document.body.appendChild(btn);
                        return btn;
                    }
                    ensureElement('chat-captcha-element');
                    var btn = ensureTrigger();

                    window.__glm_captcha_param = null;
                    window.__glm_captcha_fail = null;
                    window.__glm_captcha_err = null;
                    window.__glm_captcha_init_done = false;
                    window.initAliyunCaptcha({
                        SceneId: 'didk33e0',
                        mode: 'popup',
                        element: '#chat-captcha-element',
                        button: '#chat-captcha-trigger',
                        language: 'en',
                        timeout: 20000,
                        delayBeforeSuccess: false,
                        prefix: 'a',
                        success: function(p) { window.__glm_captcha_param = p; },
                        fail: function(e) { window.__glm_captcha_fail = JSON.stringify(e); },
                        onError: function(e) { window.__glm_captcha_err = JSON.stringify(e); }
                    }, function() {});
                    btn.click();
                    window.__glm_captcha_init_done = true;
                    return { init: true };
                } catch (e) {
                    return { error: 'init threw: ' + String(e) };
                }
            })()
        "##;
        let init_result = self.sessions.execute_js(&self.session_id, init_js).await
            .map_err(|e| DirectError::Fallback(format!("captcha init exec failed: {e}")))?;
        if let Some(err) = init_result.get("error").and_then(|v| v.as_str()) {
            warn!(captcha_init_error = %err, "GLM captcha init returned error");
            if err.contains("initAliyunCaptcha not defined") {
                return Err(DirectError::Fallback(format!("captcha init failed: {err}")));
            }
        }

        // Poll for the InitCaptchaV3 response in the lock-free store.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            self.sessions.pump_event_loop(&self.session_id, 500).await.ok();

            // Check JS outcome — the SDK might have already called success
            let poll_js = r##"(function() {
                var p = window.__glm_captcha_param;
                if (p != null && typeof p === 'object' && Object.keys(p).length > 0) {
                    return { done: true, param: p };
                }
                if (typeof p === 'string' && p.length > 0) {
                    return { done: true, param: p };
                }
                var e = window.__glm_captcha_err;
                if (e) return { done: true, error: e };
                return { done: false };
            })()"##;
            let poll_result = self.sessions.execute_js(&self.session_id, poll_js).await
                .unwrap_or_default();
            if poll_result.get("done").and_then(|v| v.as_bool()).unwrap_or(false) {
                if let Some(param) = poll_result.get("param") {
                    let param_str = param.to_string();
                    let preview = param_str.chars().take(120).collect::<String>();
                    info!(param_preview = %preview, "GLM captcha param obtained from JS success callback");
                    return Ok(param.clone());
                }
                if let Some(err) = poll_result.get("error").and_then(|v| v.as_str()) {
                    warn!(captcha_js_error = %err, "GLM captcha JS error");
                }
            }

            // Check lock-free captcha body (populated by on_response callback)
            if let Ok(Some(body)) = self.sessions.get_captcha_response_body(&self.session_id).await {
                if let Ok(body_str) = std::str::from_utf8(&body) {
                    warn!(raw_captcha_body = %body_str.chars().take(300).collect::<String>(), "GLM captcha raw response body");
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body_str) {
                        // Try to extract SessionId from InitCaptchaV3 response
                        let session_id = parsed
                            .get("SessionId")
                            .or_else(|| parsed.pointer("/CaptchaResult/SessionId"))
                            .and_then(|v| v.as_str());
                        if let Some(sid) = session_id {
                            let token_json = serde_json::json!({
                                "sessionId": sid,
                                "sceneId": "didk33e0",
                                "appName": "saf-captcha-waf",
                            });
                            let token_str = serde_json::to_string(&token_json).unwrap_or_default();
                            let param = base64::engine::general_purpose::STANDARD.encode(token_str.as_bytes());
                            info!(session_id = %sid, "GLM captcha param constructed from SessionId");
                            return Ok(serde_json::Value::String(param));
                        }
                        // Also try CertifyId (from slider solve response — unexpected without slider)
                        let certify_id = parsed
                            .get("CertifyId")
                            .or_else(|| parsed.pointer("/Result/CertifyId"))
                            .and_then(|v| v.as_str());
                        if let Some(cid) = certify_id {
                            let token_json = serde_json::json!({
                                "certifyId": cid,
                                "sceneId": "didk33e0",
                                "isSign": true
                            });
                            let token_str = serde_json::to_string(&token_json).unwrap_or_default();
                            let param = base64::engine::general_purpose::STANDARD.encode(token_str.as_bytes());
                            info!(certify_id = %cid, "GLM captcha param constructed from CertifyId");
                            return Ok(serde_json::Value::String(param));
                        }
                        let keys: Vec<String> = match &parsed {
                            serde_json::Value::Object(m) => m.keys().cloned().collect(),
                            _ => vec!["not_an_object".to_string()],
                        };
                        warn!(response_keys = %keys.join(","), "GLM captcha response has no SessionId");
                    }
                }
            }
        }

        Err(DirectError::Fallback(
            "GLM captcha solve timed out".to_string(),
        ))
    }

    /// Look up stored tool calls for this chat and format the results.
    async fn handle_tool_results(&self, chat_id: &str, messages: &[ChatMessage]) -> String {
        let tool_msgs: Vec<&ChatMessage> = messages.iter().filter(|m| m.role == "tool").collect();
        if tool_msgs.is_empty() {
            return String::new();
        }

        let mut items: Vec<(Option<ToolCall>, Option<String>, String)> = Vec::new();
        for msg in tool_msgs {
            let call = match msg.tool_call_id.as_deref() {
                Some(id) => self.store.get_tool_call(chat_id, id).await,
                None => None,
            };
            items.push((call, msg.tool_call_id.clone(), msg.content.as_text()));
        }

        let refs: Vec<(Option<&ToolCall>, Option<&str>, &str)> = items
            .iter()
            .map(|(c, id, o)| (c.as_ref(), id.as_deref(), o.as_str()))
            .collect();

        format_tool_results(&refs)
    }
}

fn build_upstream_url(
    base: &str,
    auth: &AuthContext,
    signature: &super::signature::Signature,
    chat_id: &str,
    fingerprint: &Fingerprint,
) -> Result<String, GatewayError> {
    let mut url = url::Url::parse(base)
        .map_err(|e| GatewayError::Internal(format!("invalid GLM upstream URL: {e}")))?;

    let current_url = format!("{}/c/{}", CHAT_Z_AI_URL, chat_id);
    let pathname = format!("/c/{}", chat_id);

    let now = std::time::SystemTime::now();
    let local_time = if fingerprint.local_time.is_empty() {
        chrono::DateTime::<chrono::Local>::from(now)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    } else {
        fingerprint.local_time.clone()
    };
    let utc_time = if fingerprint.utc_time.is_empty() {
        chrono::DateTime::<chrono::Utc>::from(now)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    } else {
        fingerprint.utc_time.clone()
    };

    let host = if fingerprint.host.is_empty() {
        "chat.z.ai"
    } else {
        fingerprint.host.as_str()
    };
    let hostname = if fingerprint.hostname.is_empty() {
        host
    } else {
        fingerprint.hostname.as_str()
    };
    let protocol = if fingerprint.protocol.is_empty() {
        "https"
    } else {
        fingerprint.protocol.as_str()
    };

    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("timestamp", &signature.timestamp_ms.to_string())
            .append_pair("requestId", &signature.request_id)
            .append_pair("user_id", &auth.user_id)
            .append_pair("version", "1.1.76")
            .append_pair("platform", "web")
            .append_pair("token", &auth.token)
            .append_pair("current_url", &current_url)
            .append_pair("pathname", &pathname)
            .append_pair("signature_timestamp", &signature.timestamp_ms.to_string())
            // Browser fingerprint values extracted from the warmed session.
            .append_pair("user_agent", &fingerprint.user_agent)
            .append_pair("language", &fingerprint.language)
            .append_pair("languages", &fingerprint.languages)
            .append_pair("timezone", &fingerprint.timezone)
            .append_pair("cookie_enabled", &fingerprint.cookie_enabled)
            .append_pair("screen_width", &fingerprint.screen_width)
            .append_pair("screen_height", &fingerprint.screen_height)
            .append_pair("screen_resolution", &fingerprint.screen_resolution)
            .append_pair("viewport_height", &fingerprint.viewport_height)
            .append_pair("viewport_width", &fingerprint.viewport_width)
            .append_pair("viewport_size", &fingerprint.viewport_size)
            .append_pair("color_depth", &fingerprint.color_depth)
            .append_pair("pixel_ratio", &fingerprint.pixel_ratio)
            .append_pair("search", "")
            .append_pair("hash", "")
            .append_pair("host", host)
            .append_pair("hostname", hostname)
            .append_pair("protocol", protocol)
            .append_pair("referrer", &fingerprint.referrer)
            .append_pair("title", &fingerprint.title)
            .append_pair("timezone_offset", &fingerprint.timezone_offset)
            .append_pair("local_time", &local_time)
            .append_pair("utc_time", &utc_time)
            .append_pair("is_mobile", &fingerprint.is_mobile)
            .append_pair("is_touch", &fingerprint.is_touch)
            .append_pair("max_touch_points", &fingerprint.max_touch_points)
            .append_pair("browser_name", &fingerprint.browser_name)
            .append_pair("os_name", &fingerprint.os_name);
    }

    Ok(url.to_string())
}

/// Resolve a public model id to the upstream internal id using the model
/// list returned by `/api/models`. Falls back to the public id if no mapping
/// is present.
fn find_internal_id(models: &serde_json::Value, public_id: &str) -> Option<String> {
    let arr = models.as_array()?;
    for entry in arr {
        let obj = entry.as_object()?;
        let id = obj.get("id")?.as_str()?;
        if id != public_id {
            continue;
        }
        if let Some(internal) = obj
            .get("model_id")
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("internal_id").and_then(|v| v.as_str()))
        {
            return Some(internal.to_string());
        }
        return Some(id.to_string());
    }
    None
}

/// Returns true when an upstream text payload indicates a recoverable
/// fingerprint/captcha/version challenge that should trigger UI fallback.
fn is_fallback_error_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("captcha")
        || lower.contains("refresh")
        || lower.contains("version")
        || lower.contains("更新")
        || lower.contains("刷新")
        || lower.contains("重试")
}

/// Extract a top-level application error message from an `/api/models` response.
fn extract_app_error(body: &serde_json::Value) -> Option<String> {
    body.get("error")
        .and_then(|e| e.get("detail").and_then(|v| v.as_str()))
        .or_else(|| body.get("error").and_then(|e| e.get("message").and_then(|v| v.as_str())))
        .or_else(|| body.get("detail").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

/// Split the first SSE event from the rest of the response body. Uses the
/// same `"\n\n"` / `"\r\n"` boundary convention as the streaming loop so
/// the remaining bytes can be fed straight back into it.
fn split_first_sse_event(body: &[u8]) -> (Option<crate::providers::glm::rpc::UpstreamEvent>, Vec<u8>) {
    for i in 0..body.len().saturating_sub(1) {
        let w = &body[i..i + 2];
        if w == b"\n\n" || w == b"\r\n" {
            let prefix = &body[..i];
            let event = crate::providers::glm::rpc::parse_sse_line(
                std::str::from_utf8(prefix).unwrap_or(""),
            );
            let remaining = body[i + 2..].to_vec();
            return (event, remaining);
        }
    }
    let event = crate::providers::glm::rpc::parse_sse_line(
        std::str::from_utf8(body).unwrap_or(""),
    );
    (event, Vec::new())
}

fn extract_chat_id(session_url: &str) -> Option<String> {
    let prefix = format!("{}/c/", CHAT_Z_AI_URL);
    session_url
        .strip_prefix(&prefix)
        .or_else(|| session_url.strip_prefix("zai://session/"))
        .map(|s| s.split('?').next().unwrap_or(s).to_string())
}

fn inject_tool_context(messages: &mut [ChatMessage], context: &str) {
    if let Some(idx) = messages.iter().rposition(|m| m.role == "user") {
        let original = messages[idx].content.as_text();
        messages[idx].content = crate::models::ChatContent::String(format!("{}\n\n{}", context, original));
    }
}

fn update_last_user_text(messages: &mut [ChatMessage], text: &str) {
    if let Some(idx) = messages.iter().rposition(|m| m.role == "user") {
        messages[idx].content = crate::models::ChatContent::String(text.to_string());
    }
}

fn send_content_chunk(
    tx: &mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
    counter: &AtomicU32,
    id_prefix: &str,
    model: &str,
    delta: &str,
    session_url: &Option<String>,
    emitted_role: &mut bool,
) -> Result<(), ()> {
    counter.fetch_add(1, Ordering::Relaxed);
    let role = if !*emitted_role {
        *emitted_role = true;
        Some("assistant".to_string())
    } else {
        None
    };
    tx.send(Ok(ChatCompletionChunk {
        id: format!("{}-{}", id_prefix, counter.load(Ordering::Relaxed)),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta {
                role,
                content: Some(delta.to_string()),
                reasoning_content: None,
                citations: None,
                tool_calls: None,
            },
            finish_reason: None,
        }],
        session_url: session_url.clone(),
    }))
    .map_err(|_| ())
}

fn send_reasoning_chunk(
    tx: &mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
    counter: &AtomicU32,
    id_prefix: &str,
    model: &str,
    delta: &str,
    session_url: &Option<String>,
) -> Result<(), ()> {
    counter.fetch_add(1, Ordering::Relaxed);
    tx.send(Ok(ChatCompletionChunk {
        id: format!("{}-{}", id_prefix, counter.load(Ordering::Relaxed)),
        object: "chat.completion.chunk".to_string(),
        created: current_timestamp(),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChatMessageDelta {
                role: None,
                content: None,
                reasoning_content: Some(delta.to_string()),
                citations: None,
                tool_calls: None,
            },
            finish_reason: None,
        }],
        session_url: session_url.clone(),
    }))
    .map_err(|_| ())
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Fetch and cache the Aliyun Captcha SDK content. The SDK is patched to
/// expose its internal configuration (`Rt` / `Kt`) on `window.__ALIYUN_RT`
/// so we can read the appKey for later invocations.
///
/// The SDK is executed as plain JS in Obscura's V8 because dynamically-
/// created `<script>` elements are not processed by the script loader.
async fn fetch_captcha_sdk() -> Result<String, GatewayError> {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(cached) = CACHE.get() {
        return Ok(cached.clone());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| GatewayError::Internal(format!("SDK fetch client: {e}")))?;
    let resp = client
        .get("https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js")
        .send()
        .await
        .map_err(|e| GatewayError::Internal(format!("SDK fetch: {e}")))?;
    let text = resp
        .text()
        .await
        .map_err(|e| GatewayError::Internal(format!("SDK read: {e}")))?;
    if text.is_empty() {
        return Err(GatewayError::Internal(
            "Aliyun captcha SDK returned empty body".to_string(),
        ));
    }

    // Patch the SDK to:
    // 1. Expose `Kt` (aliased as `Rt`) on window so we can read the
    //    appKey / endpoints mapping for init and debugging.
    // 2. Replace `pr` (which throws an uncatchable Error) with a no-op
    //    so that the dynamic-JS-failure path can complete gracefully.
    //    The failure path calls `er.success()` (via `s()`) right before
    //    `pr("networkError")`, so silencing the throw lets our global
    //    outcome variable survive.
    // 3. Disable the feiLin device-module loader (`en` / `nn`) because
    //    it tries to append `<script>` elements that Obscura does not
    //    execute.  The device config is not needed for our workflow.
    let patched = text
        .replace("var Rt=Kt", "window.__ALIYUN_RT=Kt;var Rt=Kt")
        .replace(
            "var De=er,Ie=nr,ze=et,Me=ot",
            "window.__ALIYUN_ET=et;window.__ALIYUN_ME=ot;\
             var De=er,Ie=nr,ze=et,Me=ot",
        )
        .replace(
            "pr=function(t){throw new Error({networkError:\"Network Error\"}[t])}",
            "pr=function(t){console.warn('[Obscura] captcha suppressed:',t)}",
        )
        .replace(
            "function en(t,r,e,n){return nn.apply(this,arguments)}",
            "function en(){}",
        )
        // 4. Fix the URL construction: when `v` (the prefix) is empty
        //    the SDK produces `.captcha-open.aliyuncs.com` which fails
        //    DNS in Obscura.  Only prepend `v + "."` when v is non-empty.
        .replace(
            "function(t){return v+\".\"+t}",
            "function(t){return v?v+\".\"+t:t}",
        )
        .replace(
            "function(t){return r._prefix+\".\"+t}",
            "function(t){return r._prefix?r._prefix+\".\"+t:t}",
        );

    let _ = CACHE.set(patched.clone());
    Ok(patched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::signature::Signature;

    fn dummy_auth() -> AuthContext {
        AuthContext {
            token: "token-123".to_string(),
            user_id: "user-123".to_string(),
        }
    }

    fn dummy_signature() -> Signature {
        Signature {
            value: "sig".to_string(),
            timestamp_ms: 1_700_000_000_000,
            request_id: "req-123".to_string(),
        }
    }

    #[test]
    fn upstream_url_includes_fingerprint_params() {
        let fp = Fingerprint {
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36".into(),
            language: "en-US".into(),
            languages: "en-US,en".into(),
            timezone: "Europe/Berlin".into(),
            cookie_enabled: "true".into(),
            screen_width: "2560".into(),
            screen_height: "1440".into(),
            screen_resolution: "2560x1440".into(),
            viewport_width: "2560".into(),
            viewport_height: "1400".into(),
            viewport_size: "2560x1400".into(),
            color_depth: "24".into(),
            pixel_ratio: "2".into(),
            host: "chat.z.ai".into(),
            hostname: "chat.z.ai".into(),
            protocol: "https".into(),
            referrer: "https://example.com/".into(),
            title: "Z.ai".into(),
            timezone_offset: "-60".into(),
            local_time: "2026-07-14 14:00:00".into(),
            utc_time: "2026-07-14 12:00:00".into(),
            is_mobile: "false".into(),
            is_touch: "false".into(),
            max_touch_points: "0".into(),
            browser_name: "Chrome".into(),
            os_name: "Win32".into(),
        };
        let url = build_upstream_url(
            "https://chat.z.ai/api/v2/chat/completions",
            &dummy_auth(),
            &dummy_signature(),
            "chat-abc",
            &fp,
        )
        .unwrap();

        assert!(url.contains("requestId=req-123"), "missing requestId");
        assert!(url.contains("user_id=user-123"), "missing user_id");
        assert!(url.contains("token=token-123"), "missing token");
        assert!(url.contains("platform=web"), "missing platform");
        assert!(url.contains("user_agent=Mozilla"), "missing user_agent");
        assert!(url.contains("timezone=Europe%2FBerlin"), "missing timezone");
        assert!(url.contains("screen_resolution=2560x1440"), "missing screen_resolution");
        assert!(url.contains("browser_name=Chrome"), "missing browser_name");
        assert!(url.contains("os_name=Win32"), "missing os_name");
        assert!(url.contains("pathname=%2Fc%2Fchat-abc"), "missing pathname");
    }

    #[test]
    fn find_internal_id_prefers_model_id() {
        let models = serde_json::json!([
            {"id": "glm-5.2", "model_id": "glm-5.2-internal"},
            {"id": "glm-5.1", "internal_id": "GLM-5.1"},
        ]);
        assert_eq!(
            find_internal_id(&models, "glm-5.2"),
            Some("glm-5.2-internal".to_string())
        );
        assert_eq!(
            find_internal_id(&models, "glm-5.1"),
            Some("GLM-5.1".to_string())
        );
    }

    #[test]
    fn find_internal_id_falls_back_to_public_id() {
        let models = serde_json::json!([{"id": "glm-5.2"}]);
        assert_eq!(
            find_internal_id(&models, "glm-5.2"),
            Some("glm-5.2".to_string())
        );
    }

    #[test]
    fn find_internal_id_returns_none_when_missing() {
        let models = serde_json::json!([{"id": "glm-5.2"}]);
        assert_eq!(find_internal_id(&models, "unknown"), None);
    }

    #[test]
    fn fallback_error_text_detects_captcha_and_version() {
        assert!(is_fallback_error_text("FRONTEND_CAPTCHA_REQUIRED"));
        assert!(is_fallback_error_text("请刷新页面以更新应用后重试"));
        assert!(!is_fallback_error_text("random server error"));
    }

    #[test]
    fn extract_app_error_parses_detail_and_message() {
        let body = serde_json::json!({"error": {"detail": "captcha required"}});
        assert_eq!(extract_app_error(&body), Some("captcha required".to_string()));

        let body = serde_json::json!({"error": {"message": "invalid token"}});
        assert_eq!(extract_app_error(&body), Some("invalid token".to_string()));
    }
}
