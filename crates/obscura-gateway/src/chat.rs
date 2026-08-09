//! Chat completion loop, shared by every provider.
//!
//! The [`Provider`] trait only declares selectors and a done signal. The
//! actual driving of the browser — filling the input, clicking submit,
//! polling the rendered DOM for text growth — lives here and is identical
//! across providers.
//!
//! The two public entry points are:
//! - [`chat`] — non-streaming, returns a complete [`ChatCompletionResponse`].
//! - [`chat_stream`] — streaming, returns an SSE-friendly stream of
//!   [`ChatCompletionChunk`]s.
//!
//! Both share [`run_chat_with_callback`], the inner loop that takes an
//! `on_delta` callback so the streaming and non-streaming paths only
//! differ in how they surface the deltas.

use std::time::{Duration, Instant};

use futures::stream::{self as futures_stream, BoxStream, StreamExt};
use tracing::{debug, info, warn};

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionChoice, ChatCompletionRequest, ChatCompletionResponse,
    ChatContent, ChatMessage, ChatMessageDelta, ChunkChoice, Usage,
};
use crate::providers::{DoneSignal, Provider};
use crate::session::SessionManager;

/// Maximum total time a single chat request is allowed to take, including
/// pre-prompt, fill, click, and the entire poll loop.
const HARD_DEADLINE: Duration = Duration::from_secs(180);

/// Time to wait between the input-fill eval and the submit-click eval so
/// that React/Preact processes the `input` event before we attempt to
/// click the (still-disabled) submit button.
const REACT_SETTLE: Duration = Duration::from_millis(300);

/// Poll interval for the visible-text extraction loop.
const POLL_INTERVAL: Duration = Duration::from_millis(120);

/// Default "text stable" duration for [`DoneSignal::TextStable`] providers
/// that don't override it. 1.5s comfortably exceeds a single render frame
/// without making the user wait too long after generation finishes.
#[allow(dead_code)]
const DEFAULT_TEXT_STABLE: Duration = Duration::from_millis(1500);

/// A single piece of new text observed during polling.
#[derive(Debug, Clone)]
pub enum Delta {
    /// New characters appended to the response container.
    Content(String),
    /// New characters appended to the reasoning / thinking panel.
    Reasoning(String),
}

/// Result of [`run_chat_with_callback`].
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Final visible content of the response container.
    pub content: String,
    /// Final visible content of the reasoning panel (empty when the
    /// provider has no `thinking_selector`).
    pub thinking: String,
    /// Whether the loop completed normally (true) or hit a deadline / error.
    pub complete: bool,
}

// ============================================================================
// Public entry points
// ============================================================================

/// Non-streaming chat completion. Returns a fully-assembled
/// [`ChatCompletionResponse`] once the response has finished rendering.
pub async fn chat(
    sessions: &SessionManager,
    provider: &dyn Provider,
    request: ChatCompletionRequest,
) -> Result<ChatCompletionResponse, GatewayError> {
    let prompt_chars = request
        .messages
        .iter()
        .map(|m| m.content.len())
        .sum::<usize>();

    let mut collected_content = String::new();
    let mut collected_thinking = String::new();
    let mut on_delta = |delta: Delta| match delta {
        Delta::Content(s) => collected_content.push_str(&s),
        Delta::Reasoning(s) => collected_thinking.push_str(&s),
    };

    let result = run_chat_with_callback(sessions, provider, &request, &mut on_delta).await?;
    debug!(
        model = %request.model,
        content_len = result.content.len(),
        thinking_len = result.thinking.len(),
        complete = result.complete,
        "chat finished",
    );

    let content_len = result.content.len();
    let content = result.content;
    let finish_reason = if result.complete { "stop" } else { "length" };
    Ok(ChatCompletionResponse {
        id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: current_timestamp(),
        model: request.model.clone(),
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: ChatContent::String(content),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: finish_reason.to_string(),
        }],
        usage: Usage {
            prompt_tokens: (prompt_chars / 4) as i32,
            completion_tokens: (content_len / 4) as i32,
            total_tokens: ((prompt_chars + content_len) / 4) as i32,
        },
        session_url: None,
    })
}

/// Streaming chat completion. Returns a stream of [`ChatCompletionChunk`]s
/// in OpenAI SSE format, including a final `finish_reason: "stop"` chunk.
pub async fn chat_stream(
    sessions: &SessionManager,
    provider: std::sync::Arc<dyn Provider>,
    request: ChatCompletionRequest,
) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
    let model = request.model.clone();
    let id_prefix = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let mut counter: u32 = 0;

    // Build the chunk stream on top of a channel so we can move the
    // `run_chat_with_callback` future onto a background task and yield
    // chunks as they arrive.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

    // The spawned future owns everything it needs. SessionManager is
    // clone-on-write (Arc internally), the provider is passed as an Arc,
    // and the request is moved by value.
    let sessions = sessions.clone();

    tokio::spawn(async move {
        let mut on_delta = |delta: Delta| {
            counter += 1;
            let chunk = ChatCompletionChunk {
                id: format!("{}-{}", id_prefix, counter),
                object: "chat.completion.chunk".to_string(),
                created: current_timestamp(),
                model: model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: match delta {
                        Delta::Content(s) => ChatMessageDelta {
                            role: if counter == 1 { Some("assistant".to_string()) } else { None },
                            content: Some(s),
                            reasoning_content: None,
                            citations: None,
                            tool_calls: None,
                        },
                        Delta::Reasoning(s) => ChatMessageDelta {
                            role: None,
                            content: None,
                            reasoning_content: Some(s),
                            citations: None,
                            tool_calls: None,
                        },
                    },
                    finish_reason: None,
                }],
                session_url: None,
            };
            let _ = tx.send(Ok(chunk));
        };

        let result = run_chat_with_callback(&sessions, provider.as_ref(), &request, &mut on_delta).await;

        match result {
            Ok(_) => {
                let _ = tx.send(Ok(ChatCompletionChunk {
                    id: format!("{}-final", id_prefix),
                    object: "chat.completion.chunk".to_string(),
                    created: current_timestamp(),
                    model,
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChatMessageDelta::default(),
                        finish_reason: Some("stop".to_string()),
                    }],
                    session_url: None,
                }));
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        }
    });

    Ok(futures_stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
    .boxed())
}

// ============================================================================
// Inner loop
// ============================================================================

/// Drive a single chat request end-to-end: acquire a session, run
/// pre-prompt JS, fill input, click submit, poll for visible text growth,
/// and detect the configured [`DoneSignal`].
///
/// `on_delta` is invoked once per observed growth in either the response
/// or thinking container.
async fn run_chat_with_callback(
    sessions: &SessionManager,
    provider: &dyn Provider,
    request: &ChatCompletionRequest,
    on_delta: &mut (dyn FnMut(Delta) + Send),
) -> Result<RunResult, GatewayError> {
    let deadline = Instant::now() + HARD_DEADLINE;
    let session = sessions.acquire().await?;

    // Build the user prompt from the last user message in the request.
    let prompt = last_user_message(&request.messages).unwrap_or_default();

    // Use a scoped guard so we always release the session, even on error.
    let outcome = drive_chat_inner(
        sessions,
        provider,
        &session.id,
        &prompt,
        request,
        deadline,
        on_delta,
    )
    .await;

    let dirty = outcome.is_err();
    if let Err(e) = sessions.release(session.id.clone(), dirty).await {
        warn!(error = %e, session_id = %session.id, "failed to release session");
    }

    outcome
}

#[allow(clippy::too_many_arguments)]
async fn drive_chat_inner(
    sessions: &SessionManager,
    provider: &dyn Provider,
    session_id: &str,
    prompt: &str,
    request: &ChatCompletionRequest,
    deadline: Instant,
    on_delta: &mut (dyn FnMut(Delta) + Send),
) -> Result<RunResult, GatewayError> {
    // UI providers share a physical browser pool. Always establish the
    // provider's approved chat origin before touching its DOM so one
    // provider's page state cannot be submitted to another provider.
    sessions.navigate(session_id, provider.url()).await?;

    // 1. Pre-prompt JS (synchronous). Providers use this for "New Chat",
    //    model-picker clicks, etc.
    if let Some(js) = provider.pre_prompt_js() {
        let result = sessions.execute_js(session_id, js).await?;
        debug!(?result, "pre-prompt JS result");
    }

    // 2. Fill the chat input via a single sync eval.
    let input_selectors_json = serde_json::to_string(&provider.input_selectors())
        .map_err(|e| GatewayError::Internal(format!("serialize selectors: {e}")))?;
    let prompt_json = serde_json::to_string(prompt)
        .map_err(|e| GatewayError::Internal(format!("serialize prompt: {e}")))?;

    let fill_js = format!(
        r#"
        (function() {{
            try {{
                var input = null;
                var tsels = {input_selectors_json};
                for (var i = 0; i < tsels.length && !input; i++) {{
                    input = document.querySelector(tsels[i]);
                }}
                if (!input) {{
                    return {{ok: false, error: 'no input found'}};
                }}
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
                return {{ok: true, value: input.value, text: input.textContent}};
            }} catch (e) {{
                return {{ok: false, error: String(e)}};
            }}
        }})()
        "#,
    );
    let fill_result = sessions.execute_js(session_id, &fill_js).await?;
    ensure_ok(&fill_result, "fill input")?;

    // 3. Brief settle so React/Preact processes the input event and the
    //    submit button enables.
    tokio::time::sleep(REACT_SETTLE).await;

    // 4. Click submit via a single sync eval.
    let submit_selectors_json = serde_json::to_string(&provider.submit_selectors())
        .map_err(|e| GatewayError::Internal(format!("serialize selectors: {e}")))?;
    let click_js = format!(
        r#"
        (function() {{
            try {{
                var btn = null;
                var bsels = {submit_selectors_json};
                for (var j = 0; j < bsels.length && !btn; j++) {{
                    btn = document.querySelector(bsels[j]);
                }}
                if (!btn) {{
                    return {{ok: false, error: 'no submit button'}};
                }}
                if (btn.disabled) {{
                    return {{ok: false, error: 'submit button disabled'}};
                }}
                // Dispatch a realistic pointer + click sequence and mark events
                // as trusted so the page accepts them as real user input.
                btn.focus();
                var mark = globalThis.__obscura_markTrusted || function(ev) {{ return ev; }};
                var rect = btn.getBoundingClientRect();
                var x = rect.left + rect.width / 2;
                var y = rect.top + rect.height / 2;
                var opts = {{bubbles: true, cancelable: true, composed: true, view: window, clientX: x, clientY: y, screenX: x, screenY: y, pointerId: 1, pointerType: 'mouse', isPrimary: true}};
                btn.dispatchEvent(mark(new PointerEvent('pointerdown', opts)));
                btn.dispatchEvent(mark(new PointerEvent('pointerup', opts)));
                btn.dispatchEvent(mark(new MouseEvent('mousedown', opts)));
                btn.dispatchEvent(mark(new MouseEvent('mouseup', opts)));
                btn.dispatchEvent(mark(new MouseEvent('click', opts)));
                // Also dispatch Enter key on the textarea as a fallback send trigger.
                var input2 = null;
                var isels = {input_selectors_json};
                for (var k = 0; k < isels.length && !input2; k++) {{
                    input2 = document.querySelector(isels[k]);
                }}
                if (input2) {{
                    input2.dispatchEvent(mark(new KeyboardEvent('keydown', {{bubbles: true, cancelable: true, key: 'Enter', code: 'Enter', keyCode: 13, which: 13}})));
                }}
                return {{ok: true}};
            }} catch (e) {{
                return {{ok: false, error: String(e)}};
            }}
        }})()
        "#,
    );
    let click_result = sessions.execute_js(session_id, &click_js).await?;
    ensure_ok(&click_result, "click submit")?;

    info!(
        session_id = %session_id,
        model = %request.model,
        prompt_chars = prompt.len(),
        "chat submitted",
    );

    // 5. Poll loop.
    let mut last_response = String::new();
    let mut last_thinking = String::new();
    let mut last_growth = Instant::now();
    let mut saw_any_text = false;

    loop {
        if Instant::now() >= deadline {
            warn!(session_id = %session_id, "hard deadline reached");
            return Ok(RunResult {
                content: last_response,
                thinking: last_thinking,
                complete: false,
            });
        }

        let texts = sessions
            .extract_texts(
                session_id,
                provider.response_selector(),
                provider.thinking_selector(),
            )
            .await?;

        if texts.response != last_response {
            // Diff and emit.
            let delta = diff_suffix(&last_response, &texts.response);
            if !delta.is_empty() {
                on_delta(Delta::Content(delta));
                last_growth = Instant::now();
                saw_any_text = true;
            }
            last_response = texts.response;
        }

        if texts.thinking != last_thinking {
            let delta = diff_suffix(&last_thinking, &texts.thinking);
            if !delta.is_empty() {
                on_delta(Delta::Reasoning(delta));
            }
            last_thinking = texts.thinking;
        }

        // Done-signal check.
        if saw_any_text && is_done(provider, sessions, session_id, &last_response, last_growth).await? {
            return Ok(RunResult {
                content: last_response,
                thinking: last_thinking,
                complete: true,
            });
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn is_done(
    provider: &dyn Provider,
    sessions: &SessionManager,
    session_id: &str,
    last_response: &str,
    last_growth: Instant,
) -> Result<bool, GatewayError> {
    if last_response.is_empty() {
        return Ok(false);
    }
    match provider.done_signal() {
        DoneSignal::TextStable(d) => {
            let stable_for = d;
            Ok(last_growth.elapsed() >= stable_for)
        }
        DoneSignal::SelectorDisappears(sel) => {
            // Done when the selector no longer matches any visible element.
            Ok(!sessions.is_visible(session_id, sel).await.unwrap_or(true))
        }
    }
}

/// Compute the suffix that turns `prev` into `next`, assuming `next` is a
/// pure extension of `prev` (the typical streaming-text case). If `next`
/// diverges (e.g. the provider re-rendered and replaced the text), we fall
/// back to returning the full new content with a leading newline.
pub(crate) fn diff_suffix(prev: &str, next: &str) -> String {
    if next.starts_with(prev) {
        next[prev.len()..].to_string()
    } else if prev.starts_with(next) {
        // Text shrank — provider probably collapsed a placeholder. No delta.
        String::new()
    } else {
        // Re-render: prefix with a newline so downstream consumers see a
        // paragraph break rather than a mid-word join.
        let mut out = String::with_capacity(next.len() + 1);
        if !next.is_empty() {
            out.push('\n');
            out.push_str(next);
        }
        out
    }
}

pub fn last_user_message(messages: &[crate::models::ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_text())
}

/// Validate that an eval result is `{ok: true}` and otherwise surface the
/// provider-side error message.
pub fn ensure_ok(result: &serde_json::Value, op: &str) -> Result<(), GatewayError> {
    if let Some(obj) = result.as_object() {
        if obj.get("ok").and_then(|v| v.as_bool()) == Some(false) {
            let err = obj
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(GatewayError::Provider(format!("{op} failed: {err}")));
        }
    }
    Ok(())
}

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_suffix_pure_extension() {
        assert_eq!(diff_suffix("hello", "hello world"), " world");
        assert_eq!(diff_suffix("", "hi"), "hi");
        assert_eq!(diff_suffix("abc", "abc"), "");
    }

    #[test]
    fn diff_suffix_shrink_emits_nothing() {
        // Provider collapsed placeholder text. No delta.
        assert_eq!(diff_suffix("Loading...", ""), "");
    }

    #[test]
    fn diff_suffix_rerender_inserts_newline() {
        let d = diff_suffix("Loading...", "The answer is 42");
        assert_eq!(d, "\nThe answer is 42");
    }

    #[test]
    fn last_user_message_picks_last_user_role() {
        let msgs = vec![
            ChatMessage {
                role: "system".into(),
                content: "you are an assistant".into(),
                name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        },
            ChatMessage {
                role: "user".into(),
                content: "first".into(),
                name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        },
            ChatMessage {
                role: "assistant".into(),
                content: "hi".into(),
                name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        },
            ChatMessage {
                role: "user".into(),
                content: "second".into(),
                name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        },
        ];
        assert_eq!(last_user_message(&msgs), Some("second".to_string()));
    }

    #[test]
    fn last_user_message_no_user_returns_none() {
        let msgs = vec![ChatMessage {
            role: "system".into(),
            content: "system".into(),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        assert_eq!(last_user_message(&msgs), None);
    }
}
