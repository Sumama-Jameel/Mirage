//! GLM UI-automation fallback backend.
//!
//! Drives the authenticated `chat.z.ai` web page directly. This is the legacy
//! path that is now invoked only when the direct internal-API path cannot be
//! used (missing token, signature failure, captcha challenge, etc.).

use std::time::Duration;

use base64::Engine as _;
use futures::stream::{BoxStream, StreamExt};
use reqwest::header::CONTENT_TYPE;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, info, warn};

use crate::chat::{diff_suffix, last_user_message};
use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatMessageDelta, ChunkChoice, ToolCall, Usage,
};
use crate::providers::tool_call::{
    convert_xml_tool_calls, format_tool_results, inject_tool_prompt,
};
use crate::session::{SessionHandle, SessionManager};

use super::humanize::{ANTI_DETECTION_INIT_JS, HUMAN_MOUSE_MOVE_JS};
use super::models::GlmModelDef;
use super::response::parse_response_body;
use super::state::SessionStore;

pub(crate) const CHAT_Z_AI_URL: &str = "https://chat.z.ai";
pub(crate) const CHAT_URL_PREFIX: &str = "https://chat.z.ai/c/";
pub(crate) const TIMEOUT_SECS: u64 = 180;
pub(crate) const CAPTURE_URL_PATTERN: &str = "/api/";

/// CSS selectors used to extract streamed text from the chat.z.ai page.
pub(crate) const RESPONSE_SELECTOR: &str =
    "[data-message-author-role='assistant'], .assistant-message, .markdown-body, [data-testid='assistant-message'], .message-assistant";
pub(crate) const THINKING_SELECTOR: &str = "[data-testid*='thinking' i], [class*='thinking' i], details[type='reasoning']";

/// How often the streaming loop polls the DOM for new visible text.
pub(crate) const STREAM_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// Hard deadline for the streaming loop.
pub(crate) const STREAM_HARD_DEADLINE: Duration = Duration::from_secs(TIMEOUT_SECS);

/// Output of the shared setup that runs before either `run_glm_chat` or
/// `run_glm_chat_stream` waits for the response.
pub(crate) struct GlmRequestSetup {
    /// Resolved chat id. Empty when the request started a fresh chat.
    pub chat_id: String,
    /// Prompt that was actually submitted to the chat box.
    #[allow(dead_code)]
    pub prompt: String,
    /// Capture handle for the in-flight `/api/` response.
    pub capture_handle: crate::session::CaptureHandle,
}

/// Run a non-streaming GLM chat via the web UI and capture the full response.
pub(crate) async fn run_glm_chat(
    sessions: &SessionManager,
    session: &SessionHandle,
    store: &SessionStore,
    model: &GlmModelDef,
    request: ChatCompletionRequest,
) -> Result<ChatCompletionResponse, GatewayError> {
    let prompt_chars = request
        .messages
        .iter()
        .map(|m| m.content.len())
        .sum::<usize>();

    let setup = setup_glm_request(sessions, session, store, model, &request).await?;

    let response_bytes = wait_for_capture(sessions, &session.id, setup.capture_handle).await?;

    let mut response_data = parse_response_body(&response_bytes, &request.model)?;

    if response_data.tool_calls.is_none() && request.tools.is_some() {
        let (cleaned, parsed) = convert_xml_tool_calls(&response_data.text, true);
        if let Some(calls) = parsed {
            if !calls.is_empty() {
                response_data.tool_calls = Some(calls);
                response_data.text = cleaned;
            }
        }
    }

    let final_chat_id = response_data.chat_id.unwrap_or_else(|| setup.chat_id.clone());

    if !final_chat_id.is_empty() {
        store.insert(final_chat_id.clone(), &model.id).await;

        if let Some(ref calls) = response_data.tool_calls {
            if !calls.is_empty() {
                store.store_tool_calls(&final_chat_id, calls).await;
            }
        }
    }

    let content_len = response_data.text.len();
    let reasoning_len = response_data
        .thinking
        .as_ref()
        .map(|t| t.len())
        .unwrap_or(0);
    let has_tool_calls = response_data.tool_calls.is_some();

    Ok(ChatCompletionResponse {
        id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: current_timestamp(),
        model: request.model.clone(),
        choices: vec![crate::models::ChatCompletionChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: crate::models::ChatContent::String(response_data.text),
                name: None,
                reasoning_content: response_data.thinking,
                citations: response_data.citations,
                tool_calls: response_data.tool_calls,
                tool_call_id: None,
            },
            finish_reason: if has_tool_calls {
                "tool_calls".to_string()
            } else {
                "stop".to_string()
            },
        }],
        usage: Usage {
            prompt_tokens: (prompt_chars / 4) as i32,
            completion_tokens: ((content_len + reasoning_len) / 4) as i32,
            total_tokens: ((prompt_chars + content_len + reasoning_len) / 4) as i32,
        },
        session_url: if final_chat_id.is_empty() {
            None
        } else {
            Some(format!("{CHAT_URL_PREFIX}{final_chat_id}"))
        },
    })
}

/// Run a streaming GLM chat via the web UI.
pub(crate) async fn run_glm_chat_stream(
    sessions: &SessionManager,
    session: &SessionHandle,
    store: &SessionStore,
    model: &GlmModelDef,
    request: ChatCompletionRequest,
) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
    let setup = setup_glm_request(sessions, session, store, model, &request).await?;

    let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

    let sessions_for_task = sessions.clone();
    let session_id = session.id.clone();
    let store_for_task = store.clone();
    let model_id = model.id.clone();
    let request_for_task = request;
    let capture_handle = setup.capture_handle;
    let initial_chat_id = setup.chat_id;

    tokio::spawn(async move {
        let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let mut id_counter: u32 = 0;
        let mut emitted_role = false;
        let mut last_response = String::new();
        let mut last_thinking = String::new();
        let mut last_growth = tokio::time::Instant::now();
        let mut saw_text = false;
        let deadline = tokio::time::Instant::now() + STREAM_HARD_DEADLINE;
        let stable_for = Duration::from_millis(2000);

        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!("GLM UI stream: hard deadline reached during DOM polling");
                break;
            }

            match sessions_for_task
                .extract_texts(&session_id, RESPONSE_SELECTOR, Some(THINKING_SELECTOR))
                .await
            {
                Ok(texts) => {
                    if texts.response != last_response {
                        let delta = diff_suffix(&last_response, &texts.response);
                        if !delta.is_empty() {
                            saw_text = true;
                            last_growth = tokio::time::Instant::now();
                            id_counter += 1;
                            let role = if !emitted_role {
                                emitted_role = true;
                                Some("assistant".to_string())
                            } else {
                                None
                            };
                            let chunk = ChatCompletionChunk {
                                id: format!("{id_prefix}-{id_counter}"),
                                object: "chat.completion.chunk".to_string(),
                                created: current_timestamp(),
                                model: model_id.clone(),
                                choices: vec![ChunkChoice {
                                    index: 0,
                                    delta: ChatMessageDelta {
                                        role,
                                        content: Some(delta),
                                        reasoning_content: None,
                                        citations: None,
                                        tool_calls: None,
                                    },
                                    finish_reason: None,
                                }],
                                session_url: None,
                            };
                            if tx.send(Ok(chunk)).is_err() {
                                return;
                            }
                        }
                        last_response = texts.response;
                    }

                    if texts.thinking != last_thinking {
                        let delta = diff_suffix(&last_thinking, &texts.thinking);
                        if !delta.is_empty() {
                            id_counter += 1;
                            let chunk = ChatCompletionChunk {
                                id: format!("{id_prefix}-{id_counter}"),
                                object: "chat.completion.chunk".to_string(),
                                created: current_timestamp(),
                                model: model_id.clone(),
                                choices: vec![ChunkChoice {
                                    index: 0,
                                    delta: ChatMessageDelta {
                                        role: None,
                                        content: None,
                                        reasoning_content: Some(delta),
                                        citations: None,
                                        tool_calls: None,
                                    },
                                    finish_reason: None,
                                }],
                                session_url: None,
                            };
                            if tx.send(Ok(chunk)).is_err() {
                                return;
                            }
                        }
                        last_thinking = texts.thinking;
                    }

                    if saw_text && last_growth.elapsed() >= stable_for {
                        break;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "GLM UI stream: DOM polling error (will retry)");
                }
            }

            tokio::time::sleep(STREAM_POLL_INTERVAL).await;
        }

        let response_bytes = match wait_for_capture(
            &sessions_for_task,
            &session_id,
            capture_handle,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };

        let mut response_data = match parse_response_body(&response_bytes, &model_id) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };

        let mut tool_calls = response_data.tool_calls;
        if tool_calls.is_none() && request_for_task.tools.is_some() {
            let (cleaned, parsed) = convert_xml_tool_calls(&response_data.text, true);
            if let Some(calls) = parsed {
                if !calls.is_empty() {
                    tool_calls = Some(calls);
                    response_data.text = cleaned;
                }
            }
        }

        let final_chat_id = response_data
            .chat_id
            .unwrap_or_else(|| initial_chat_id.clone());

        if !final_chat_id.is_empty() {
            store_for_task.insert(final_chat_id.clone(), &model_id).await;
            if let Some(ref calls) = tool_calls {
                if !calls.is_empty() {
                    store_for_task.store_tool_calls(&final_chat_id, calls).await;
                }
            }
        }

        let session_url = if final_chat_id.is_empty() {
            None
        } else {
            Some(format!("{CHAT_URL_PREFIX}{final_chat_id}"))
        };

        if let Some(ref cites) = response_data.citations {
            if !cites.is_empty() {
                id_counter += 1;
                let chunk = ChatCompletionChunk {
                    id: format!("{id_prefix}-{id_counter}"),
                    object: "chat.completion.chunk".to_string(),
                    created: current_timestamp(),
                    model: model_id.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChatMessageDelta {
                            role: None,
                            content: None,
                            reasoning_content: None,
                            citations: Some(cites.clone()),
                            tool_calls: None,
                        },
                        finish_reason: None,
                    }],
                    session_url: session_url.clone(),
                };
                if tx.send(Ok(chunk)).is_err() {
                    return;
                }
            }
        }

        if let Some(ref calls) = tool_calls {
            if !calls.is_empty() {
                id_counter += 1;
                let chunk = ChatCompletionChunk {
                    id: format!("{id_prefix}-{id_counter}"),
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
            }
        }

        let has_tool_calls = tool_calls.as_ref().is_some_and(|c| !c.is_empty());
        let finish_reason = if has_tool_calls {
            "tool_calls".to_string()
        } else {
            "stop".to_string()
        };
        let _ = tx.send(Ok(ChatCompletionChunk {
            id: format!("{id_prefix}-final"),
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

async fn setup_glm_request(
    sessions: &SessionManager,
    session: &SessionHandle,
    store: &SessionStore,
    model: &GlmModelDef,
    request: &ChatCompletionRequest,
) -> Result<GlmRequestSetup, GatewayError> {
    let chat_id = resolve_or_create_chat(store, &request.session_url, &model.id).await?;

    let tool_result_context = if !chat_id.is_empty() {
        handle_tool_results(store, &chat_id, &request.messages).await
    } else {
        String::new()
    };

    let base_user_prompt = last_user_message(&request.messages).unwrap_or_default();
    let base_prompt = if tool_result_context.is_empty() {
        base_user_prompt
    } else {
        format!("{tool_result_context}\n\n{base_user_prompt}")
    };

    let prompt = match &request.tools {
        Some(tools) => inject_tool_prompt(&base_prompt, tools, request.tool_choice.as_ref()),
        None => base_prompt,
    };

    let capture_handle = sessions.start_capture(&session.id, CAPTURE_URL_PATTERN).await?;

    let navigate_url = if chat_id.is_empty() {
        CHAT_Z_AI_URL.to_string()
    } else {
        format!("{CHAT_URL_PREFIX}{chat_id}")
    };

    sessions.navigate(&session.id, &navigate_url).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Inject anti-detection scripts before any interaction
    inject_anti_detection(sessions, &session.id).await;
    inject_humanize(sessions, &session.id).await;

    ensure_input_ready(sessions, &session.id).await?;

    set_features_via_js(sessions, &session.id, model, request).await;

    prepare_attachments(sessions, &session.id, request).await;

    fill_and_submit(sessions, &session.id, &prompt).await?;

    Ok(GlmRequestSetup {
        chat_id,
        prompt,
        capture_handle,
    })
}

async fn handle_tool_results(
    store: &SessionStore,
    chat_id: &str,
    messages: &[ChatMessage],
) -> String {
    let tool_msgs: Vec<&ChatMessage> = messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();

    if tool_msgs.is_empty() {
        return String::new();
    }

    let mut items: Vec<(Option<ToolCall>, Option<String>, String)> = Vec::new();
    for msg in &tool_msgs {
        let call = match msg.tool_call_id.as_deref() {
            Some(id) => store.get_tool_call(chat_id, id).await,
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

async fn resolve_or_create_chat(
    store: &SessionStore,
    session_url: &Option<String>,
    model_id: &str,
) -> Result<String, GatewayError> {
    match session_url.as_deref() {
        Some(url) => {
            let chat_id = url
                .strip_prefix(CHAT_URL_PREFIX)
                .or_else(|| url.strip_prefix("zai://session/"))
                .map(|s| s.split('?').next().unwrap_or(s).to_string())
                .ok_or_else(|| {
                    GatewayError::BadRequest(format!("invalid GLM session_url: {url}"))
                })?;
            let _ = store
                .acquire(&chat_id)
                .await
                .ok_or_else(|| {
                    GatewayError::BadRequest(format!("expired GLM session_url: {url}"))
                })?;
            SessionStore::ensure_model_matches(model_id, &store.get_model(&chat_id).await.unwrap_or_default())?;
            Ok(chat_id)
        }
        None => Ok(String::new()),
    }
}

async fn ensure_input_ready(
    sessions: &SessionManager,
    session_id: &str,
) -> Result<(), GatewayError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        let result = sessions
            .execute_js(
                session_id,
                r##"
                (function() {
                    // chat.z.ai uses a ProseMirror-style rich-text editor.
                    // The "real" input is a div[contenteditable] that may not
                    // have a standard role or placeholder attribute.  Broaden
                    // the search to cover more editor variants.
                    var selectors = [
                        "#chat-input",
                        "textarea[placeholder*='Ask' i]",
                        "textarea[placeholder*='Message' i]",
                        "textarea[placeholder*='message' i]",
                        "div[contenteditable='true'][role='textbox']",
                        "div[contenteditable='true']",
                        "div.ProseMirror",
                        "div[data-editor]",
                        "[role='textbox']",
                        "textarea",
                    ];
                    for (var i = 0; i < selectors.length; i++) {
                        var el = document.querySelector(selectors[i]);
                        if (el) return {ok: true, tag: el.tagName, selector: selectors[i]};
                    }
                    return {ok: false};
                })()
                "##,
            )
            .await?;
        if let Some(ok) = result.get("ok").and_then(|v| v.as_bool()) {
            if ok {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(GatewayError::Provider(
        "chat.z.ai input textarea not found within timeout".to_string(),
    ))
}

async fn inject_anti_detection(
    sessions: &SessionManager,
    session_id: &str,
) {
    let js = format!(
        r#"(function() {{
            try {{
                {anti_js}
                return {{ ok: true }};
            }} catch (e) {{
                return {{ ok: false, error: String(e) }};
            }}
        }})()"#,
        anti_js = ANTI_DETECTION_INIT_JS
    );
    match sessions.execute_js(session_id, &js).await {
        Ok(result) => {
            debug!(result = ?result, "GLM anti-detection init result");
        }
        Err(e) => {
            warn!(error = %e, "GLM anti-detection init failed; continuing");
        }
    }
}

async fn inject_humanize(
    sessions: &SessionManager,
    session_id: &str,
) {
    let js = format!(
        r#"(function() {{
            try {{
                {human_js}
                return {{ ok: true }};
            }} catch (e) {{
                return {{ ok: false, error: String(e) }};
            }}
        }})()"#,
        human_js = HUMAN_MOUSE_MOVE_JS
    );
    match sessions.execute_js(session_id, &js).await {
        Ok(result) => {
            debug!(result = ?result, "GLM humanize init result");
        }
        Err(e) => {
            warn!(error = %e, "GLM humanize init failed; continuing");
        }
    }
}

async fn set_features_via_js(
    sessions: &SessionManager,
    session_id: &str,
    model: &GlmModelDef,
    request: &ChatCompletionRequest,
) {
    let thinking = request.thinking.unwrap_or(false);
    let search = request.search.unwrap_or(false);

    let js = format!(
        r#"
        (function() {{
            try {{
                var key = null;
                for (var i = 0; i < localStorage.length; i++) {{
                    var k = localStorage.key(i);
                    if (k && (k.startsWith('chat:') || k.indexOf('chat') !== -1)) {{ key = k; break; }}
                }}
                if (!key) return {{ok: false, error: 'no active chat in localStorage'}};
                var raw = localStorage.getItem(key);
                var chat = raw ? JSON.parse(raw) : null;
                if (!chat || !chat.chat) return {{ok: false, error: 'chat object missing'}};
                chat.chat.enable_thinking = {thinking};
                chat.chat.reasoning_effort = {reasoning_effort};
                chat.chat.auto_web_search = {search};
                if (Array.isArray(chat.chat.features)) {{
                    chat.chat.features = chat.chat.features.filter(function(f) {{
                        return f && f.type !== 'web_search' && f.type !== 'thinking';
                    }});
                }}
                localStorage.setItem(key, JSON.stringify(chat));
                return {{ok: true}};
            }} catch (e) {{
                return {{ok: false, error: String(e)}};
            }}
        }})()
        "#,
        thinking = if thinking { "true" } else { "false" },
        reasoning_effort = if thinking { "'max'" } else { "'high'" },
        search = if search { "true" } else { "false" },
    );

    match sessions.execute_js(session_id, &js).await {
        Ok(result) => {
            debug!(result = ?result, "chat.z.ai feature setter result");
        }
        Err(e) => {
            warn!(error = %e, model = %model.id, "feature setter JS failed; using defaults");
        }
    }
}

async fn prepare_attachments(
    sessions: &SessionManager,
    session_id: &str,
    request: &ChatCompletionRequest,
) {
    let last_msg = match request.messages.last() {
        Some(m) => m,
        None => return,
    };
    let image_urls = last_msg.content.image_urls();
    let file_urls = last_msg.content.file_urls();
    if image_urls.is_empty() && file_urls.is_empty() {
        return;
    }

    // Resolve every URL into (base64, mime, filename). Data URIs are decoded
    // directly; remote URLs are downloaded via reqwest with SSRF protection.
    // We always end up with bytes, which the page can attach via a DataTransfer
    // without needing CORS access to the remote host.
    let mut items: Vec<serde_json::Value> = Vec::new();
    for url in image_urls.iter().chain(file_urls.iter()) {
        match resolve_attachment(url).await {
            Ok((b64, mime, filename)) => {
                items.push(serde_json::json!({
                    "b64": b64,
                    "mime": mime,
                    "filename": filename,
                }));
            }
            Err(e) => {
                warn!(url = %url, error = %e, "failed to fetch attachment for UI fallback");
            }
        }
    }

    if items.is_empty() {
        warn!("no attachments could be resolved for UI fallback");
        return;
    }

    info!(
        count = items.len(),
        "chat.z.ai UI attachment: pushing bytes into page and attaching via input[type=file]"
    );

    // Step 1: stash the resolved attachments on a page global via a sync eval.
    let payload = match serde_json::to_string(&items) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to serialize UI attachments payload");
            return;
        }
    };
    let seed_js = format!(
        r#"
        (function() {{
            try {{
                window.__obscura_attachments = {payload};
                return {{ ok: true, count: {count} }};
            }} catch (e) {{
                return {{ ok: false, error: String(e) }};
            }}
        }})()
        "#,
        payload = payload,
        count = items.len(),
    );
    if let Err(e) = sessions.execute_js(session_id, &seed_js).await {
        warn!(error = %e, "failed to seed attachment global; UI attachment skipped");
        return;
    }

    // Step 2: attach each item through the page's file input.
    let attach_js = r#"
    (async function() {
        function b64ToBlob(b64, mime) {
            var bin = atob(b64);
            var arr = new Uint8Array(bin.length);
            for (var i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
            return new Blob([arr], { type: mime || 'application/octet-stream' });
        }
        var items = window.__obscura_attachments || [];
        var input = document.querySelector('input[type="file"]');
        if (!input) return { ok: false, error: 'no file input on page', attached: 0 };
        var attached = 0;
        var errors = [];
        for (var i = 0; i < items.length; i++) {
            try {
                var it = items[i];
                var blob = b64ToBlob(it.b64, it.mime);
                var file = new File([blob], it.filename, { type: blob.type });
                var dt = new DataTransfer();
                dt.items.add(file);
                input.files = dt.files;
                input.dispatchEvent(new Event('change', { bubbles: true }));
                attached++;
                // Yield to the page so it can process each file individually.
                await new Promise(function(r) { setTimeout(r, 200); });
            } catch (e) {
                errors.push(String(e));
            }
        }
        window.__obscura_attachments = null;
        return { ok: attached > 0, attached: attached, errors: errors };
    })()
    "#;

    match sessions
        .execute_js_async(session_id, attach_js)
        .await
    {
        Ok(value) => debug!(result = ?value, "UI attachment result"),
        Err(e) => warn!(error = %e, "UI attachment async eval failed"),
    }
}

/// Resolve an attachment URL into (base64, mime, filename).
async fn resolve_attachment(url: &str) -> Result<(String, String, String), GatewayError> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, b64) = rest.split_once(',').ok_or_else(|| {
            GatewayError::BadRequest("invalid data URI: missing comma separator".to_string())
        })?;
        let mime = meta
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .to_string();
        let filename = format!("upload.{}", extension_for_mime(&mime));
        return Ok((b64.to_string(), mime, filename));
    }

    super::validate_remote_url(url)?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| GatewayError::Internal(format!("attachment client failed: {e}")))?;

    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("attachment download failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(GatewayError::Provider(format!(
            "attachment download returned {}",
            resp.status()
        )));
    }

    let mime = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .to_string();

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Provider(format!("attachment body read failed: {e}")))?;

    let parsed = url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|s| s.last())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("upload.{}", extension_for_mime(&mime)));

    Ok((base64::engine::general_purpose::STANDARD.encode(&bytes), mime, parsed))
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "application/json" => "json",
        _ => "bin",
    }
}

async fn fill_and_submit(
    sessions: &SessionManager,
    session_id: &str,
    prompt: &str,
) -> Result<(), GatewayError> {
    let prompt_json = serde_json::to_string(prompt)
        .map_err(|e| GatewayError::Internal(format!("serialize prompt: {e}")))?;

    let fill_js = format!(
        r##"
        (function() {{
            try {{
                var input = null;
                var selectors = [
                    "#chat-input",
                    "textarea[placeholder*='Ask' i]",
                    "textarea[placeholder*='Message' i]",
                    "textarea[placeholder*='message' i]",
                    "[contenteditable='true'][role='textbox']",
                    "textarea"
                ];
                for (var i = 0; i < selectors.length && !input; i++) {{
                    input = document.querySelector(selectors[i]);
                }}
                if (!input) return {{ok: false, error: 'no input found'}};
                var prompt = {prompt_json};
                if (input.tagName === 'TEXTAREA' || input.tagName === 'INPUT') {{
                    var proto = input.tagName === 'TEXTAREA'
                        ? window.HTMLTextAreaElement.prototype
                        : window.HTMLInputElement.prototype;
                    var setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
                    input.focus();
                    setter.call(input, prompt);
                    var mark = globalThis.__obscura_markTrusted || function(ev) {{ return ev; }};
                    input.dispatchEvent(mark(new Event('input', {{bubbles: true}})));
                    input.dispatchEvent(mark(new Event('change', {{bubbles: true}})));
                    input.dispatchEvent(mark(new KeyboardEvent('keyup', {{bubbles: true, key: 'Enter', code: 'Enter'}})));
                }} else {{
                    input.focus();
                    input.textContent = prompt;
                    var mark = globalThis.__obscura_markTrusted || function(ev) {{ return ev; }};
                    input.dispatchEvent(mark(new Event('input', {{bubbles: true}})));
                    input.dispatchEvent(mark(new Event('change', {{bubbles: true}})));
                }}
                return {{ok: true}};
            }} catch (e) {{
                return {{ok: false, error: String(e)}};
            }}
        }})()
        "##
    );

    let fill_result = sessions.execute_js(session_id, &fill_js).await?;
    crate::chat::ensure_ok(&fill_result, "fill input")?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let click_js = r##"
        (async function() {
            try {
                var input = null;
                var inputSelectors = [
                    "#chat-input",
                    "textarea[placeholder*='Ask' i]",
                    "textarea[placeholder*='Message' i]",
                    "[contenteditable='true'][role='textbox']",
                    "textarea"
                ];
                for (var s = 0; s < inputSelectors.length && !input; s++) {
                    input = document.querySelector(inputSelectors[s]);
                }
                var btn = null;
                var selectors = [
                    "#send-button",
                    "button[aria-label*='send' i]",
                    "button[type='submit']",
                    "button svg[viewBox]",
                    "button:has(svg)"
                ];
                // Last resort: the last enabled button inside the composer/form that contains the input.
                if (!btn && input) {
                    var composer = input.closest('form, [class*="composer"], [class*="input"]');
                    if (composer) {
                        var buttons = composer.querySelectorAll('button');
                        for (var k = buttons.length - 1; k >= 0; k--) {
                            if (!buttons[k].disabled) { btn = buttons[k]; break; }
                        }
                    }
                }
                for (var j = 0; j < selectors.length && !btn; j++) {
                    btn = document.querySelector(selectors[j]);
                }
                if (!btn) return {ok: false, error: 'no submit button'};
                if (btn.disabled) return {ok: false, error: 'submit button disabled'};
                btn.focus();

                // Use human-like click with Bézier curve movement
                if (window.__obscura_humanize && window.__obscura_humanize.click) {
                    var result = await window.__obscura_humanize.click(btn);
                    return {ok: result};
                }

                // Fallback to direct click if humanize not loaded
                var mark = globalThis.__obscura_markTrusted || function(ev) { return ev; };
                var rect = btn.getBoundingClientRect();
                var x = rect.left + rect.width / 2;
                var y = rect.top + rect.height / 2;
                var opts = {bubbles: true, cancelable: true, composed: true, view: window, clientX: x, clientY: y, screenX: x, screenY: y, pointerId: 1, pointerType: 'mouse', isPrimary: true};
                btn.dispatchEvent(mark(new PointerEvent('pointerdown', opts)));
                btn.dispatchEvent(mark(new PointerEvent('pointerup', opts)));
                btn.dispatchEvent(mark(new MouseEvent('mousedown', opts)));
                btn.dispatchEvent(mark(new MouseEvent('mouseup', opts)));
                btn.dispatchEvent(mark(new MouseEvent('click', opts)));
                return {ok: true};
            } catch (e) {
                return {ok: false, error: String(e)};
            }
        })()
    "##;

    let click_result = sessions.execute_js_async(session_id, click_js).await?;
    crate::chat::ensure_ok(&click_result, "click submit")?;

    Ok(())
}

async fn wait_for_capture(
    sessions: &SessionManager,
    session_id: &str,
    handle: crate::session::CaptureHandle,
) -> Result<Vec<u8>, GatewayError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(TIMEOUT_SECS);
    let mut last_len = 0;
    let mut stable_for = tokio::time::Instant::now();

    while tokio::time::Instant::now() < deadline {
        let responses = handle.take_responses().await;
        if let Some(resp) = responses
            .into_iter()
            .find(|r| r.url.contains("/chat/completions") || r.url.contains("/completions"))
        {
            let len = resp.body.len();
            if len > 0 && len == last_len && stable_for.elapsed() >= Duration::from_millis(1500) {
                sessions.stop_capture(session_id).await.ok();
                return Ok(resp.body);
            }
            if len != last_len {
                last_len = len;
                stable_for = tokio::time::Instant::now();
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    sessions.stop_capture(session_id).await.ok();
    Err(GatewayError::Provider(
        "timed out waiting for chat.z.ai response".to_string(),
    ))
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
