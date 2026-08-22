//! GLM/Z.AI direct internal-API client.
//!
//! Makes direct HTTP requests to `chat.z.ai/api/v2/chat/completions` using
//! the imported browser session for authentication and request signing. Unlike
//! the previous in-page `globalThis.fetch()` approach, the server-side HTTP
//! path avoids triggering Aliyun NVC captcha (verified by zai2api and other
//! reverse-engineering projects).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream::{BoxStream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info, warn};

use obscura_net::CookieJar;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatMessageDelta, ChunkChoice, Citation, ToolCall, Usage,
};
use crate::providers::retry_after_from_map;
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::mtp;
use crate::providers::mtp_pipeline::MtpPipeline;
use crate::session::{SessionHandle, SessionManager};

use obscura_net::StealthHttpClient;

use super::auth::{resolve_auth, AuthContext};
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
#[allow(dead_code)]
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
    stealth: StealthHttpClient,
    upload_http: reqwest::Client,
    auth: AuthContext,
    store: SessionStore,
    model: GlmModelDef,
    sign_secret: String,
    upstream_url: String,
    sessions: SessionManager,
    session_id: String,
    user_agent: String,
    profile_path: Option<std::path::PathBuf>,
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
        profile_path: Option<&std::path::Path>,
    ) -> Result<Self, DirectError> {
        let auth = resolve_auth(&session.local_storage, &session.cookie_jar)
            .await
            .map_err(|e| DirectError::Fallback(e.to_string()))?;

        // Use wreq with Chrome 145 TLS fingerprint to bypass Z.AI's
        // Aliyun NVC captcha. The standard reqwest client has a non-browser
        // JA3 fingerprint that triggers FRONTEND_CAPTCHA_REQUIRED.
        let upload_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .build()
            .map_err(|e| GatewayError::Internal(format!("GLM upload client failed: {e}")))?;

        // Use wreq with Chrome 145 TLS fingerprint. Do NOT set extra headers
        // here — wreq's emulation already sends correct browser fingerprint
        // headers (UA, Accept, sec-ch-ua, etc.). Adding our own overrides
        // can conflict with the emulated fingerprint and trigger captcha.
        let stealth = StealthHttpClient::new(session.cookie_jar.clone());

        info!(
            session_id = %session.id,
            model = %model.id,
            "GLM DirectClient initialized"
        );

        Ok(Self {
            stealth,
            upload_http,
            auth,
            store,
            model: model.clone(),
            sign_secret: sign_secret.to_string(),
            upstream_url: upstream_url.to_string(),
            sessions: sessions.clone(),
            session_id: session.id.clone(),
            user_agent: session.user_agent.clone(),
            profile_path: profile_path.map(std::path::PathBuf::from),
            models_cache: std::sync::Mutex::new(None),
        })
    }

    /// Warm the browser to the target chat URL and probe `/api/models` to
    /// validate the session and resolve the requested public model id to the
    /// upstream internal id. The model list is cached for the lifetime of this
    /// client (one gateway request).
    async fn resolve_internal_model_id(
        &self,
        _chat_id: &str,
    ) -> Result<String, DirectError> {
        // Current chat.z.ai v2 accepts the public model id directly. The
        // separate /api/models probe belongs to the older signed contract;
        // it can hang when called outside the page's authenticated fetch
        // context and prevents the actual completion request from being
        // issued. The current web executor uses the requested model id
        // unchanged (OmniRoute zai-web, 2026-08-16 research).
        if self.upstream_url.ends_with("/api/v2/chat/completions") {
            return Ok(self.model.id.clone());
        }

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
        let mut probe_headers = std::collections::HashMap::new();
        probe_headers.insert("Authorization".to_string(), format!("Bearer {}", self.auth.token));
        probe_headers.insert("X-FE-Version".to_string(), X_FE_VERSION.to_string());
        let probe_url_parsed = url::Url::parse(&probe_url)
            .map_err(|e| DirectError::Fatal(GatewayError::Internal(format!("invalid GLM models URL: {e}"))))?;
        let resp = self
            .stealth
            .send_single("GET", &probe_url_parsed, &probe_headers, "")
            .await
            .map_err(|e| DirectError::Fallback(format!("GLM model probe request failed: {e}")))?;

        let status_u16 = resp.status;
        if status_u16 == 401 || status_u16 == 403 {
            return Err(DirectError::Fallback(format!(
                "GLM model probe returned {status_u16}; session invalid or captcha required"
            )));
        }
        if status_u16 == 400 || status_u16 == 429 || status_u16 == 426 {
            let text = String::from_utf8_lossy(&resp.body).to_string();
            if is_fallback_error_text(&text) {
                return Err(DirectError::Fallback(format!(
                    "GLM model probe returned {status_u16}: {text}"
                )));
            }
            let retry_after = retry_after_from_map(&resp.headers);
            if status_u16 == 429 {
                return Err(DirectError::Fatal(GatewayError::ProviderRateLimited {
                    message: format!("GLM model probe returned 429: {}", text.chars().take(500).collect::<String>()),
                    retry_after,
                }));
            }
            return Err(DirectError::Fatal(GatewayError::Provider(format!(
                "GLM model probe returned {status_u16}: {}",
                text.chars().take(500).collect::<String>()
            ))));
        }
        if status_u16 >= 400 {
            let text = String::from_utf8_lossy(&resp.body);
            return Err(DirectError::Fatal(GatewayError::Provider(format!(
                "GLM model probe returned {status_u16}: {}",
                text.chars().take(500).collect::<String>()
            ))));
        }

        let body: serde_json::Value = serde_json::from_slice(&resp.body).map_err(|e| {
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

        // Tool-call extraction happens in the streaming path (MTP pipeline),
        // so chunks already carry structured tool_calls.
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

        // Compile client tools into the MTP system prompt (universal
        // dialect). The prompt is prepended as a system message; native
        // tools/tool_choice are never forwarded upstream.
        let mut pipeline = MtpPipeline::prepare("glm", &model_id, &request);
        if pipeline.active {
            request.messages = pipeline.upstream_messages(&request);
            if pipeline.strip_upstream_tools() {
                request.tools = None;
                request.tool_choice = None;
            }
        }

        // Upload any attachments in the last user message. On failure, fall back
        // to UI automation which can drive the native file picker.
        let upload_service = UploadService::new(
            self.upload_http.clone(),
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

        let _has_attachments = !files.is_empty();
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

        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

        let stream_result: Result<
            BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
            DirectError,
        > = async {
            let signature = generate_signature(&self.auth.user_id, &last_user, &self.sign_secret)
                .map_err(|e| DirectError::Fatal(GatewayError::Internal(format!("GLM signature failed: {e}"))))?;
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

            let url = build_upstream_url(
                    &self.upstream_url,
                    &self.auth,
                    &signature,
                    &chat_id,
                    &fingerprint,
                )?;

                // Re-read cookies from the live Firefox profile. Z.AI's anti-bot
                // cookies (ssxmod_itna) expire within hours; the imported jar goes
                // stale. Reading from the live profile gives us fresh ones.
                if let Some(ref pf) = self.profile_path {
                    refresh_cookies_from_profile(
                        &self.stealth.cookie_jar,
                        pf,
                        "z.ai",
                    );
                }

                // Navigate to chat.z.ai to set any JS-side state.
                self.sessions.navigate(&self.session_id, CHAT_Z_AI_URL).await?;
                for _ in 0..3 {
                    self.sessions.pump_event_loop(&self.session_id, 200).await.ok();
                }

                // Make the POST request directly from the server-side HTTP
                // client. The in-page globalThis.fetch() path triggers Aliyun
                // NVC captcha because Obscura's op_fetch_url TLS fingerprint
                // differs from a real browser. zai2api and other reverse-
                // engineering projects prove that direct HTTP with the right
                // headers (X-Signature, X-FE-Version, Bearer token, cookies)
                // never triggers captcha.
                let body_json = serde_json::to_string(&body)
                    .map_err(|e| DirectError::Fatal(GatewayError::Internal(format!("JSON serialization failed: {e}"))))?;
                // zai2api does NOT send cookies, but Z.AI's server requires at
                // least the token cookie for auth. Send ONLY the auth token
                // cookie - no stale Firefox cookies that conflict with Chrome TLS.
                let token_cookie = format!("token={}", self.auth.token);

                let mut headers = std::collections::HashMap::new();
                headers.insert("Content-Type".to_string(), "application/json".to_string());
                headers.insert("Authorization".to_string(), format!("Bearer {}", self.auth.token));
                headers.insert("X-FE-Version".to_string(), X_FE_VERSION.to_string());
                headers.insert("X-Signature".to_string(), signature.value.clone());
                headers.insert("Origin".to_string(), CHAT_Z_AI_URL.to_string());
                headers.insert("Referer".to_string(), format!("{}/c/{}", CHAT_Z_AI_URL, chat_id));
                headers.insert("Connection".to_string(), "keep-alive".to_string());
                headers.insert("X-Forwarded-For".to_string(), generate_random_ip());
                headers.insert("X-Real-IP".to_string(), generate_random_ip());
                headers.insert("Cookie".to_string(), token_cookie);

                let url_parsed = url::Url::parse(&url)
                    .map_err(|e| DirectError::Fatal(GatewayError::Internal(format!("invalid GLM URL: {e}"))))?;
                let resp = self.stealth.send_single("POST", &url_parsed, &headers, &body_json).await
                    .map_err(|e| DirectError::Fallback(format!("GLM upstream request failed: {e}")))?;

                let status = resp.status;
                let body_bytes = resp.body;
                // Debug: log cookie header to diagnose captcha trigger
                info!(
                    status = status,
                    body_len = body_bytes.len(),
                    body_preview = %String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(500)]),
                    "GLM stealth HTTP response"
                );
                let (first_event, remaining_bytes) = split_first_sse_event(&body_bytes);

                // Surface non-SSE / HTTP error payloads instead of masking them
                // as an empty stream. Z.AI returns a plain "Internal Server
                // Error" body on auth/signature/param failures, which previously
                // looked like a successful empty completion.
                if status >= 400
                    || (first_event.is_none()
                        && !body_bytes.is_empty()
                        && !std::str::from_utf8(&body_bytes)
                            .unwrap_or("")
                            .trim_start()
                            .starts_with("data:"))
                {
                    let snippet = String::from_utf8_lossy(&body_bytes)
                        .chars()
                        .take(500)
                        .collect::<String>();
                    return Err(DirectError::Fallback(format!(
                        "GLM upstream error (status={}): {}",
                        status, snippet
                    )));
                }

                // The server-side HTTP path does not trigger Aliyun NVC
                // captcha (confirmed by zai2api). If it somehow does, surface
                // the error instead of retrying with in-page JS.
                if let Some(code) = first_event
                    .as_ref()
                    .and_then(|e| e.error_code.as_deref())
                {
                    if code == "FRONTEND_CAPTCHA_REQUIRED" {
                        return Err(DirectError::Fallback(
                            "GLM upstream requires captcha (server-side path). \
                             Re-import a fresh session and retry."
                                .to_string(),
                        ));
                    }
                }

                // Spawn the streaming task over the buffered bytes.
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
                            let visible = pipeline.feed(&delta);
                            if send_content_chunk(
                                &tx_clone,
                                &counter,
                                &id_prefix_clone,
                                &model_id_for_stream,
                                &visible,
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
                            let visible = pipeline.feed(&delta);
                            if send_content_chunk(
                                &tx_clone,
                                &counter,
                                &id_prefix_clone,
                                &model_id_for_stream,
                                &visible,
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
                                        let visible = pipeline.feed(&delta);
                                        if send_content_chunk(
                                            &tx_clone,
                                            &counter,
                                            &id_prefix_clone,
                                            &model_id_for_stream,
                                            &visible,
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
                                        let visible = pipeline.feed(&delta);
                                        if send_content_chunk(
                                            &tx_clone,
                                            &counter,
                                            &id_prefix_clone,
                                            &model_id_for_stream,
                                            &visible,
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

                    // Post-stream tool-call extraction: flush any pending
                    // MTP block and collect validated calls.
                    pipeline.finish();
                    let mut finish_reason = "stop".to_string();
                    let mtp_calls = pipeline.tool_calls();
                    if !mtp_calls.is_empty() {
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
                                    tool_calls: Some(mtp_calls.clone()),
                                },
                                finish_reason: None,
                            }],
                            session_url: session_url_for_stream.clone(),
                        }));
                        for call in &mtp_calls {
                            if !collected_tool_calls.iter().any(|c| c.id == call.id) {
                                collected_tool_calls.push(call.clone());
                            }
                        }
                        finish_reason = "tool_calls".to_string();
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

            Ok(UnboundedReceiverStream::new(rx).boxed())
        }
        .await;

        stream_result
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

    /// Create a chat via the real `/api/v1/chats/new` endpoint. Falls back
    /// to a local UUID on any failure so chat creation never blocks a turn
    /// (Z.AI tolerates client-minted ids, but server-created chats keep the
    /// web history consistent).
    async fn create_chat_upstream(&self, internal_model_id: &str) -> Option<String> {
        let url = format!("{CHAT_Z_AI_URL}/api/v1/chats/new");
        let body = serde_json::json!({
            "title": "New Chat",
            "models": [internal_model_id],
            "enable_thinking": self.model.id.contains("thinking"),
            "auto_web_search": false,
            "chat_type": 1,
        });
        let mut headers = std::collections::HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert("Authorization".to_string(), format!("Bearer {}", self.auth.token));
        headers.insert("Cookie".to_string(), format!("token={}", self.auth.token));
        headers.insert("Origin".to_string(), CHAT_Z_AI_URL.to_string());
        let parsed = url::Url::parse(&url).ok()?;
        let resp = self.stealth.send_single("POST", &parsed, &headers, &body.to_string()).await.ok()?;
        if resp.status != 200 {
            warn!(status = resp.status, "GLM /api/v1/chats/new failed; using local chat id");
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
        // Response shapes seen in captures: {id} or {data:{id}} or {chatId}.
        value
            .get("id")
            .or_else(|| value.get("chatId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                value
                    .get("data")
                    .and_then(|d| d.get("id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
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

        // Prefer a server-created chat; degrade to the historical local UUID.
        let internal = self.model.internal_id.clone();
        Ok(self.create_chat_upstream(&internal).await
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()))
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

        mtp::format_tool_results(&refs)
    }
}

fn build_upstream_url(
    base: &str,
    auth: &AuthContext,
    signature: &super::signature::Signature,
    _chat_id: &str,
    _fingerprint: &Fingerprint,
) -> Result<String, GatewayError> {
    let mut url = url::Url::parse(base)
        .map_err(|e| GatewayError::Internal(format!("invalid GLM upstream URL: {e}")))?;

    let mut pairs = url.query_pairs_mut();
    // Mirror the live chat.z.ai SPA request exactly. zai2api (Go, reverse-
    // engineered from Z.AI) includes these params and never triggers captcha.
    // `version` is the consumer API version (0.0.1), NOT the X-FE-Version
    // header value.
    pairs
        .append_pair("timestamp", &signature.timestamp_ms.to_string())
        .append_pair("requestId", &signature.request_id)
        .append_pair("user_id", &auth.user_id)
        .append_pair("version", "0.0.1")
        .append_pair("platform", "web")
        .append_pair("token", &auth.token)
        .append_pair("current_url", &format!("{}/c/{}", CHAT_Z_AI_URL, _chat_id))
        .append_pair("pathname", &format!("/c/{}", _chat_id))
        .append_pair("signature_timestamp", &signature.timestamp_ms.to_string());
    drop(pairs);

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

/// Generate a random public IP for X-Forwarded-For / X-Real-IP headers.
/// zai2api uses these to avoid IP-based rate limiting.
/// Re-read cookies from the live Firefox profile at request time, bypassing
/// the stale imported cookie jar. Z.AI's anti-bot service (ssxmod_itna)
/// generates session-bound cookies that expire; the imported copy goes stale
/// within hours. This re-reads the live profile's cookies.sqlite with WAL
/// replay so fresh cookies are always available.
///
/// The profile copy is private (our own temp dir), so WAL checkpoint writes
/// are safe and don't affect the running Firefox.
fn refresh_cookies_from_profile(
    cookie_jar: &CookieJar,
    profile_path: &std::path::Path,
    domain_filter: &str,
) {
    let cookies_path = profile_path.join("cookies.sqlite");
    if !cookies_path.exists() {
        warn!(
            path = %cookies_path.display(),
            "GLM cookie refresh: cookies.sqlite not found"
        );
        return;
    }

    // Copy to a private temp dir so we can WAL-checkpoint without locking the
    // live Firefox DB.
    let tmp_dir = std::env::temp_dir().join(format!("glm_cookie_refresh_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);
    let tmp_db = tmp_dir.join("cookies.sqlite");
    let tmp_wal = tmp_dir.join("cookies.sqlite-wal");
    let tmp_shm = tmp_dir.join("cookies.sqlite-shm");

    if std::fs::copy(&cookies_path, &tmp_db).is_err() {
        return;
    }
    let wal_src = profile_path.join("cookies.sqlite-wal");
    if wal_src.exists() {
        let _ = std::fs::copy(&wal_src, &tmp_wal);
    }
    let shm_src = profile_path.join("cookies.sqlite-shm");
    if shm_src.exists() {
        let _ = std::fs::copy(&shm_src, &tmp_shm);
    }

    // Open read-write to checkpoint WAL into main DB.
    use rusqlite::{Connection, OpenFlags};
    let conn = match Connection::open_with_flags(&tmp_db, OpenFlags::SQLITE_OPEN_READ_WRITE) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "GLM cookie refresh: failed to open copy");
            return;
        }
    };
    let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");

    // Read cookies matching the domain.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let sql = "SELECT name, value, host, path, expiry, isSecure FROM moz_cookies WHERE host LIKE ?1";
    let rows = match conn.prepare(sql) {
        Ok(mut stmt) => {
            let pattern = format!("%{domain_filter}%");
            stmt.query_map(rusqlite::params![pattern], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
            .unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };

    let mut updated = 0u32;
    for (name, value, host, path, expiry, _secure) in &rows {
        // Include ALL cookies, even expired ones. Z.AI's anti-bot service
        // requires ssxmod_itna/ssxmod_itna2 to EXIST regardless of expiry;
        // the anti-bot check only verifies the cookie is present, not its
        // expiry. Skipping expired cookies causes FRONTEND_CAPTCHA_REQUIRED.
        let expired = expiry.map_or(false, |exp| (exp as u64) + 60 < now);
        // Insert or update into the jar. Strip leading dot from domain
        // (".z.ai" -> "z.ai") so url::Url::parse doesn't choke.
        let clean_host = host.strip_prefix('.').unwrap_or(host);
        let url_str = format!("https://{clean_host}{path}");
        if let Ok(url) = url::Url::parse(&url_str) {
            let mut parts: Vec<String> = vec![format!("{}={}", name, value)];
            if !path.is_empty() {
                parts.push(format!("Path={}", path));
            }
            if *_secure {
                parts.push("Secure".to_string());
            }
            if !expired {
                parts.push("HttpOnly".to_string());
            }
            let cookie_str = parts.join("; ");
            cookie_jar.set_cookie(&cookie_str, &url);
            updated += 1;
        }
    }

    if updated > 0 {
        info!(updated = updated, domain = domain_filter, "GLM cookie refresh: updated from live profile");
    } else {
        warn!(domain = domain_filter, "GLM cookie refresh: no fresh cookies found in live profile");
    }

    // Clean up temp files.
    let _ = std::fs::remove_file(&tmp_db);
    let _ = std::fs::remove_file(&tmp_wal);
    let _ = std::fs::remove_file(&tmp_shm);
    let _ = std::fs::remove_dir(&tmp_dir);
}

fn generate_random_ip() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let seed = RandomState::new().build_hasher().finish();
    let first_octets: &[u8] = &[
        36, 42, 58, 60, 61, 101, 106, 110, 111, 112, 113, 114, 115, 116, 117,
        118, 119, 120, 121, 122, 123, 124, 125, 139, 140, 144, 150, 153, 157,
        163, 171, 175, 180, 182, 183, 202, 210, 211, 218, 219, 220, 221, 222, 223,
    ];
    let first = first_octets[seed as usize % first_octets.len()];
    let second = ((seed >> 8) % 256) as u8;
    let third = ((seed >> 16) % 256) as u8;
    let fourth = ((seed >> 24) % 254 + 1) as u8;
    format!("{first}.{second}.{third}.{fourth}")
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
    fn upstream_url_includes_required_params() {
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

        // Mirror the live chat.z.ai SPA request + zai2api params.
        assert!(url.contains("timestamp=1700000000000"), "missing timestamp");
        assert!(url.contains("requestId=req-123"), "missing requestId");
        assert!(url.contains("user_id=user-123"), "missing user_id");
        assert!(url.contains("version=0.0.1"), "missing version");
        assert!(url.contains("platform=web"), "missing platform");
        assert!(url.contains("token=token-123"), "missing token");
        assert!(url.contains("current_url="), "missing current_url");
        assert!(url.contains("pathname="), "missing pathname");
        assert!(url.contains("signature_timestamp="), "missing signature_timestamp");
        // Fingerprint params are NOT sent as URL query params.
        assert!(!url.contains("user_agent="), "unexpected user_agent param");
        assert!(!url.contains("timezone="), "unexpected timezone param");
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
