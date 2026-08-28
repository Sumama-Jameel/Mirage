//! Shared MTP pipeline bridging OpenAI requests and browser/internal
//! provider APIs.
//!
//! One struct drives both halves of the translation:
//!
//! * Request side: resolve the [`ProviderProfile`] for `(provider, model)`,
//!   compile client `tools` into the configured dialect's prompt, normalize
//!   history (`role:"tool"` / `assistant.tool_calls`), and report whether
//!   `tools`/`tool_choice` must be stripped from the upstream body (always,
//!   for MTP).
//! * Response side: feed streamed deltas through the dialect's parser, hide
//!   tool blocks from the user-visible text, collect OpenAI-shaped
//!   [`ToolCall`]s, and derive `finish_reason`.
//!
//! Dialect dispatch follows `profile.tool_dialect`: `Mtp` is the universal
//! default; `Xml` and `Gemini` keep the legacy parsers from
//! [`crate::providers::tool_call`] as selectable rollback dialects.
//!
//! The pipeline is the conformance-suite entry point and the dialect
//! dispatch surface; individual provider adapters may drive the underlying
//! [`mtp`] primitives directly when their transport shape demands it (flat
//! prompt providers, cumulative-text streams).

// Some accessors exist for the repair loop and diagnostics paths that only
// specific adapters exercise; they are covered by the conformance suite.
#![allow(dead_code)]

use crate::models::{ChatCompletionRequest, ChatMessage, Tool, ToolCall};

use super::mtp::{self, MtpStreamState};
use super::profile::{find_profile_by_model, ProviderProfile, Quirks, ToolDialect, Transport};
use super::tool_call::{
    gemini_tool_use_prompt, inject_tool_prompt, parse_gemini_tool_calls, XmlToolCallStripper,
};

/// Request+response pipeline for one chat turn.
pub struct MtpPipeline {
    profile: ProviderProfile,
    /// Client tool definitions (possibly truncated to `max_tools`).
    tools: Vec<Tool>,
    /// Whether the original request carried usable tools.
    pub active: bool,
    /// Stream state (dialect-specific).
    state: PipelineState,
    /// Calls collected by legacy dialects (outside MtpStreamState).
    extra_calls: Vec<ToolCall>,
    /// First invalid block cached for the repair path.
    pending_repair: Option<(String, mtp::MtpError)>,
    /// Sticky flag: any valid call seen this turn.
    saw_valid_call: bool,
    /// Repair attempts still available for invalid blocks this turn.
    repairs_left: usize,
}

#[derive(Default)]
enum PipelineState {
    #[default]
    Idle,
    Mtp(MtpStreamState),
    Xml(XmlToolCallStripper),
    /// Gemini streams re-emit cumulative text; we accumulate and parse at end.
    Gemini { full_text: String },
}

impl MtpPipeline {
    /// Build a pipeline for one request.
    ///
    /// Returns the pipeline plus the message list to send upstream (prompt
    /// injected where the dialect needs it). Callers must consult
    /// [`Self::strip_upstream_tools`] before building the upstream body and
    /// drop `request.tools`/`request.tool_choice` when it returns true.
    pub fn prepare(provider: &str, model: &str, request: &ChatCompletionRequest) -> Self {
        let profile = find_profile_by_model(model)
            .unwrap_or_else(|| ProviderProfile::new(provider, model, Transport::Sse));

        let mut tools = request.tools.clone().unwrap_or_default();
        // Cap the injected tool count for weak models.
        tools.truncate(profile.max_tools);
        let active = !tools.is_empty();

        let mut pipe = Self {
            profile,
            tools,
            active,
            state: PipelineState::Idle,
            extra_calls: Vec::new(),
            pending_repair: None,
            saw_valid_call: false,
            repairs_left: 0,
        };
        if pipe.profile.tool_dialect == ToolDialect::Mtp {
            pipe.repairs_left = pipe.profile.repair_attempts;
        }
        pipe
    }

    /// Messages to send upstream, with the dialect prompt injected.
    ///
    /// `Mtp` uses [`mtp::prepare_request`] (history normalization + system
    /// prompt); when the `ignores_system_prompt` quirk is set the compiled
    /// prompt is merged into the first user message instead. `Xml` and
    /// `Gemini` inject into the last user message, matching the legacy
    /// adapters.
    pub fn upstream_messages(&self, request: &ChatCompletionRequest) -> Vec<ChatMessage> {
        if !self.active {
            return request.messages.clone();
        }
        match self.profile.tool_dialect {
            ToolDialect::None => request.messages.clone(),
            ToolDialect::Mtp => {
                let mut messages =
                    mtp::prepare_request(request, self.profile.quirks.requires_final_reminder, self.profile.quirks.prompt_style).0;
                if self.profile.quirks.ignores_system_prompt {
                    messages = merge_system_into_first_user(messages);
                }
                messages
            }
            ToolDialect::Xml => {
                let mut messages = request.messages.clone();
                if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") {
                    let prompt = last_user.content.as_text();
                    last_user.content = inject_tool_prompt(
                        &self.profile.provider,
                        &prompt,
                        &self.tools,
                        request.tool_choice.as_ref(),
                    )
                    .into();
                }
                messages
            }
            ToolDialect::Gemini => {
                let mut messages = request.messages.clone();
                if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") {
                    let prompt = last_user.content.as_text();
                    last_user.content = gemini_tool_use_prompt(
                        &self.profile.provider,
                        &prompt,
                        &self.tools,
                        request.tool_choice.as_ref(),
                    )
                    .into();
                }
                messages
            }
        }
    }

    /// Whether `tools`/`tool_choice` must be removed from the upstream body.
    pub fn strip_upstream_tools(&self) -> bool {
        self.active && self.profile.strip_upstream_tools
    }

    /// Feed one content delta; returns user-visible cleaned text.
    pub fn feed(&mut self, delta: &str) -> String {
        if !self.active {
            return delta.to_string();
        }
        match &mut self.state {
            PipelineState::Idle => {
                let mut st = match self.profile.tool_dialect {
                    ToolDialect::Mtp => PipelineState::Mtp(MtpStreamState::new()),
                    ToolDialect::Xml => PipelineState::Xml(XmlToolCallStripper::new()),
                    ToolDialect::Gemini => PipelineState::Gemini {
                        full_text: String::new(),
                    },
                    ToolDialect::None => return delta.to_string(),
                };
                self.state = std::mem::take(&mut st);
                self.feed(delta)
            }
            PipelineState::Mtp(st) => st.process_delta(delta, &self.tools),
            PipelineState::Xml(strip) => {
                let (clean, call) = strip.process(delta);
                if let Some(call) = call {
                    self.saw_valid_call = true;
                    push_unique(&mut self.collected_from_state(), call);
                }
                clean
            }
            PipelineState::Gemini { full_text } => {
                full_text.push_str(delta);
                String::new()
            }
        }
    }

    /// Flush at stream end (parses trailing/pending blocks).
    pub fn finish(&mut self) {
        if !self.active {
            return;
        }
        match &mut self.state {
            PipelineState::Idle => {}
            PipelineState::Mtp(st) => st.finish(&self.tools),
            PipelineState::Xml(strip) => {
                if let Some(call) = strip.finish_pending() {
                    self.saw_valid_call = true;
                    let out = self.collected_from_state();
                    push_unique(out, call);
                }
            }
            PipelineState::Gemini { full_text } => {
                let text = std::mem::take(full_text);
                if let Some(calls) = parse_gemini_tool_calls(&text) {
                    if !calls.is_empty() {
                        self.saw_valid_call = true;
                    }
                    let out = self.collected_from_state();
                    for c in calls {
                        push_unique(out, c);
                    }
                }
            }
        }
    }

    /// Collected OpenAI tool calls for this turn (draining).
    pub fn tool_calls(&mut self) -> Vec<ToolCall> {
        if !self.active {
            return Vec::new();
        }
        let mut calls = match &mut self.state {
            PipelineState::Mtp(st) => std::mem::take(&mut st.collected_tool_calls),
            _ => std::mem::take(&mut self.extra_calls),
        };
        if self.profile.force_one_tool_call {
            calls.truncate(1);
        }
        if !calls.is_empty() {
            self.saw_valid_call = true;
        }
        calls
    }

    /// Peek whether any valid tool call was collected (non-draining).
    pub fn has_tool_calls(&self) -> bool {
        if !self.active {
            return false;
        }
        self.saw_valid_call
            || match &self.state {
                PipelineState::Mtp(st) => !st.collected_tool_calls.is_empty(),
                _ => !self.extra_calls.is_empty(),
            }
    }

    /// `finish_reason` for the response: `"tool_calls"` when any valid block
    /// was produced this turn, otherwise `"stop"`. Sticky: safe to call after
    /// [`Self::tool_calls`] drained the state.
    pub fn finish_reason(&self) -> &'static str {
        if self.has_tool_calls() {
            "tool_calls"
        } else {
            "stop"
        }
    }

    /// Invalid-block errors recorded during the stream.
    pub fn take_errors(&mut self) -> Vec<(String, mtp::MtpError)> {
        let errs = match &mut self.state {
            PipelineState::Mtp(st) => st.take_errors(),
            _ => Vec::new(),
        };
        if !errs.is_empty() && self.pending_repair.is_none() {
            // Cache the first failure for the repair path even after callers
            // have drained diagnostics.
            self.pending_repair = Some(errs[0].clone());
        }
        errs
    }

    /// Whether any invalid block was recorded (non-draining).
    pub fn has_errors(&self) -> bool {
        match &self.state {
            PipelineState::Mtp(st) => st.has_errors(),
            _ => false,
        }
    }

    /// Build a repair follow-up prompt for the first recorded error, if the
    /// profile's repair budget allows. Returns the follow-up user message
    /// plus the raw failed payload (for diagnostics) — or `None` when there
    /// is nothing to repair or no budget left.
    pub fn next_repair_prompt(&mut self) -> Option<(ChatMessage, String)> {
        // Ensure errors have been drained into the repair cache even if the
        // caller never inspected take_errors().
        if self.pending_repair.is_none() {
            let _ = self.take_errors();
        }
        // Peek the budget BEFORE consuming the cached entry so a zero budget
        // leaves the entry intact for diagnostics.
        if self.repairs_left == 0 || !self.pending_repair.is_some() {
            return None;
        }
        self.repairs_left -= 1;
        let (raw, err) = self.pending_repair.take()?;
        let prompt = mtp::build_repair_prompt(&err, &raw, &self.tools);
        Some((
            ChatMessage {
                role: "user".to_string(),
                content: prompt.into(),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            },
            raw.clone(),
        ))
    }

    /// Profile resolved for this turn (tests + diagnostics).
    pub fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    pub fn quirks(&self) -> Quirks {
        self.profile.quirks
    }
}

// Extra storage for legacy-dialect collected calls (XmlToolCallStripper /
// Gemini parsing produce calls outside MtpStreamState).
impl MtpPipeline {
    fn collected_from_state(&mut self) -> &mut Vec<ToolCall> {
        &mut self.extra_calls
    }
}

fn push_unique(out: &mut Vec<ToolCall>, call: ToolCall) {
    if !out.iter().any(|c| c.id == call.id && c.function.name == call.function.name) {
        out.push(call);
    }
}

/// When a model ignores system prompts, fold the MTP prompt into the first
/// user message so it actually reaches the context window.
fn merge_system_into_first_user(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    if messages.first().map(|m| m.role.as_str()) == Some("system") {
        let sys = messages.remove(0);
        if let Some(first_user) = messages.iter_mut().find(|m| m.role == "user") {
            let combined = format!("{}\n\n{}", sys.content.as_text(), first_user.content.as_text());
            first_user.content = combined.into();
        } else {
            // No user turn at all: degrade to keeping the system message.
            messages.insert(0, sys);
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FunctionDefinition;

    fn request_with_tools() -> ChatCompletionRequest {
        serde_json::from_value(serde_json::json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "write a file"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Write a file.",
                    "parameters": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}, "content": {"type": "string"}},
                        "required": ["name", "content"]
                    }
                }
            }]
        }))
        .unwrap()
    }

    #[test]
    fn mtp_prepare_prepends_system_prompt_and_strips() {
        let req = request_with_tools();
        let pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
        assert!(pipe.active);
        assert!(pipe.strip_upstream_tools());
        let msgs = pipe.upstream_messages(&req);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.as_text().contains("[MIRAGE_TOOL_CALL_V1]"));
        assert_eq!(msgs.len(), req.messages.len() + 1);
    }

    #[test]
    fn mtp_ignores_system_prompt_quirk_merges_into_user() {
        let req = request_with_tools();
        // Emulate the quirk: prepare then fold the system prompt into the
        // first user message.
        let msgs = {
            let prepared = mtp::prepare_request(&req, false, mtp::prompt_style_for_model(&req.model)).0;
            merge_system_into_first_user(prepared)
        };
        assert_eq!(msgs[0].role, "user");
        assert!(msgs[0].content.as_text().contains("CRITICAL TOOL POLICY"));
        assert!(msgs[0].content.as_text().contains("write a file"));
    }

    #[test]
    fn feed_collects_tool_block_and_finish_reason() {
        let req = request_with_tools();
        let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
        let clean = pipe.feed("here [MIRAGE_TOOL_CALL_V1]{\"name\":\"write_file\",\"arguments\":{\"name\":\"a\",\"content\":\"b\"}}[/MIRAGE_TOOL_CALL_V1]");
        pipe.finish();
        assert_eq!(clean, "here ");
        let calls = pipe.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "write_file");
        assert_eq!(pipe.finish_reason(), "tool_calls");
    }

    #[test]
    fn force_one_tool_call_truncates() {
        let req = request_with_tools();
        let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
        let block = "[MIRAGE_TOOL_CALL_V1]{\"name\":\"write_file\",\"arguments\":{\"name\":\"a\",\"content\":\"b\"}}[/MIRAGE_TOOL_CALL_V1]";
        let _ = pipe.feed(&format!("{block}x{block}"));
        pipe.finish();
        assert_eq!(pipe.tool_calls().len(), 1);
    }

    #[test]
    fn no_tools_passthrough() {
        let req: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
        assert!(!pipe.active);
        assert!(!pipe.strip_upstream_tools());
        assert_eq!(pipe.feed("plain"), "plain");
        pipe.finish();
        assert_eq!(pipe.finish_reason(), "stop");
        assert_eq!(pipe.upstream_messages(&req).len(), 1);
    }

    #[test]
    fn builtin_name_rejection_recorded_as_error() {
        let req: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "search"}],
            "tools": [{
                "type": "function",
                "function": {"name": "get_weather", "parameters": {"type": "object", "properties": {}}}
            }]
        }))
        .unwrap();
        let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
        let _ = pipe.feed("[MIRAGE_TOOL_CALL_V1]{\"name\":\"search\",\"arguments\":{}}[/MIRAGE_TOOL_CALL_V1]");
        pipe.finish();
        // Built-in-style name not in client defs: dropped, recorded, no leak.
        assert!(pipe.tool_calls().is_empty());
        let errs = pipe.take_errors();
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].1, mtp::MtpError::UnknownTool(ref n) if n == "search"));
        assert_eq!(pipe.finish_reason(), "stop");
    }

    #[test]
    fn repair_prompt_budget() {
        let req: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "x"}],
            "tools": [{
                "type": "function",
                "function": {"name": "get_weather", "parameters": {"type": "object", "properties": {}}}
            }]
        }))
        .unwrap();
        let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
        let _ = pipe.feed("[MIRAGE_TOOL_CALL_V1]{broken json[/MIRAGE_TOOL_CALL_V1]");
        pipe.finish();
        let (msg, raw) = pipe.next_repair_prompt().expect("one repair attempt");
        assert_eq!(msg.role, "user");
        assert!(msg.content.as_text().contains("YOUR PREVIOUS TOOL BLOCK WAS INVALID"));
        assert!(msg.content.as_text().contains("get_weather"));
        assert!(raw.contains("broken"));
        // Budget exhausted (repair_attempts defaults to 1).
        assert!(pipe.next_repair_prompt().is_none());
    }

    #[test]
    fn xml_dialect_dispatch_injects_legacy_prompt() {
        let req = request_with_tools();
        let pipe = MtpPipeline::prepare("legacy-provider", "unknown-model", &req);
        // Unknown model falls back to default MTP; flip dialect to verify dispatch.
        let mut pipe = pipe;
        pipe.profile.tool_dialect = ToolDialect::Xml;
        let msgs = pipe.upstream_messages(&req);
        let last = msgs.last().unwrap().content.as_text();
        assert!(last.contains("<tool_call>"), "xml prompt injected into user msg");
        assert_eq!(msgs.len(), req.messages.len());
    }
}
