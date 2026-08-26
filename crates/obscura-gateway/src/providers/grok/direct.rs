use futures::stream::{BoxStream, StreamExt};
use obscura_net::wreq_util;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatContent, ChatMessage,
    ChatMessageDelta, ChunkChoice, FunctionCall, Tool, ToolCall, Usage,
};
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::mtp;
use crate::session::SessionHandle;

use super::auth::{extract_grok_cookies, validate_grok_session};
use super::state::GrokSessionStore;
use super::upload::{self, UploadCache};

const GROK_BASE_URL: &str = "https://grok.com";
const CONVERSATIONS_PATH: &str = "/rest/app-chat/conversations/new";
const PROVIDER_NAME: &str = "grok";

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Map our model ID to the wire format `modeId` field.
fn mode_id_for_wire(model_id: &str) -> &str {
    match model_id {
        "grok-auto" | "grok-fast" => "fast",
        "grok-expert" => "expert",
        "grok-heavy" => "heavy",
        "grok-4.5" => "fast",
        "grok-4.3" => "grok-420-computer-use-sa",
        _ => {
            tracing::warn!(model = %model_id, "unknown Grok mode, falling back to fast");
            "fast"
        }
    }
}

pub fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Represents a single NDJSON event from grok.com's stream.
/// Mirrors the `result.response` object in grok.com's NDJSON format.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GrokStreamResponse {
    #[serde(default)]
    pub token: String,
    pub is_thinking: Option<bool>,
    #[serde(rename = "isSoftStop")]
    pub is_soft_stop: Option<bool>,
    #[serde(default)]
    pub message_tag: String,
    #[serde(default)]
    tool_usage_card_id: String,
    #[serde(default)]
    tool_usage_card: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct GrokStreamEvent {
    result: Option<GrokStreamEventResult>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct GrokStreamEventResult {
    response: Option<GrokStreamResponse>,
}

pub fn parse_ndjson_line(line: &str) -> Option<GrokStreamResponse> {
    let event: GrokStreamEvent = serde_json::from_str(line).ok()?;
    event.result?.response
}

/// Minimally matches a user-declared tool to a grok native tool intent.
fn tool_for_native_intent(tools: &[Tool], native_name: &str) -> Option<String> {
    let keywords: &[&str] = match native_name {
        "bash" => &["bash", "shell", "terminal", "execute", "command", "run"],
        "readFile" | "read_file" => &["read", "file_read", "read_file", "readfile"],
        "webSearch" | "web_search" => &["web_search", "websearch", "search"],
        "browsePage" | "browse_page" => &["browse", "fetch", "url_fetch", "web_fetch", "webfetch"],
        _ => return None,
    };
    for tool in tools {
        let name = tool.function.name.to_lowercase();
        if keywords.iter().any(|k| name.contains(k)) {
            return Some(tool.function.name.clone());
        }
    }
    None
}

/// Try to extract a native tool call from the structured `toolUsageCard` field.
/// Returns (tool_call_id, tool_name, args_json) if a matching user-declared tool exists.
fn parse_native_tool_card(
    resp: &GrokStreamResponse,
    tools: &[Tool],
) -> Option<(String, String, serde_json::Value)> {
    let card = resp.tool_usage_card.as_ref()?;

    let tool_id = if resp.tool_usage_card_id.is_empty() {
        format!("call_{}", new_uuid())
    } else {
        resp.tool_usage_card_id.clone()
    };

    let check = |key: &str, native_name: &str,
                 mapper: fn(&serde_json::Map<std::string::String, serde_json::Value>) -> Option<serde_json::Value>|
     -> Option<(String, String, serde_json::Value)> {
        let obj = card.get(key).and_then(|v| v.as_object())?;
        let args = mapper(obj)?;
        let tool_name = tool_for_native_intent(tools, native_name)?;
        Some((tool_id.clone(), tool_name, args))
    };

    // bash
    if let Some(result) = check("bash", "bash", |o| {
        o.get("args").map(|a| serde_json::json!(a))
    }) {
        return Some(result);
    }
    // readFile / read_file
    if let Some(result) = check("readFile", "readFile", |o| {
        let a = o.get("args")?;
        Some(serde_json::json!(a))
    }) {
        return Some(result);
    }
    if let Some(result) = check("read_file", "readFile", |o| {
        let a = o.get("args")?;
        Some(serde_json::json!(a))
    }) {
        return Some(result);
    }
    // webSearch / web_search
    if let Some(result) = check("webSearch", "webSearch", |o| {
        let a = o.get("args")?;
        Some(serde_json::json!(a))
    }) {
        return Some(result);
    }
    if let Some(result) = check("web_search", "webSearch", |o| {
        let a = o.get("args")?;
        Some(serde_json::json!(a))
    }) {
        return Some(result);
    }
    // browsePage / browse_page
    if let Some(result) = check("browsePage", "browsePage", |o| {
        let a = o.get("args")?;
        Some(serde_json::json!(a))
    }) {
        return Some(result);
    }
    if let Some(result) = check("browse_page", "browsePage", |o| {
        let a = o.get("args")?;
        Some(serde_json::json!(a))
    }) {
        return Some(result);
    }

    None
}

/// Scan for `<xai:tool_usage_card>` XML in a token string and extract the
/// tool call data. This is a fallback when the structured field is missing.
fn parse_xml_tool_card(
    text: &str,
) -> Option<(String, String, serde_json::Value)> {
    let text = text.trim();
    if !text.contains("<xai:tool_usage_card>") {
        return None;
    }

    let id = extract_xml_tag(text, "xai:tool_usage_card_id")
        .unwrap_or_else(|| format!("call_{}", new_uuid()));
    let tool_name = extract_xml_tag(text, "xai:tool_name")?;
    let cdata = extract_xml_cdata(text, "xai:tool_args")?;

    let args: serde_json::Value = serde_json::from_str(&cdata).ok()?;
    Some((id, tool_name, args))
}

fn extract_xml_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let s = text.find(&open)?;
    let e = text[s + open.len()..].find(&close)?;
    Some(text[s + open.len()..s + open.len() + e].to_string())
}

fn extract_xml_cdata(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{}><![CDATA[", tag);
    let close = format!("]]></{}>", tag);
    let s = text.find(&open)?;
    let e = text[s + open.len()..].find(&close)?;
    Some(text[s + open.len()..s + open.len() + e].to_string())
}

/// Build an OpenAI ToolCall from a native tool card extraction.
fn build_tool_call(id: &str, tool_name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: tool_name.to_string(),
            arguments: args.to_string(),
        },
    }
}

/// Build the cost estimate text for usage.
fn estimate_cost(prompt_text: &str, completion_text: &str) -> Usage {
    Usage {
        prompt_tokens: estimate_tokens(PROVIDER_NAME, "grok", prompt_text),
        completion_tokens: estimate_tokens(PROVIDER_NAME, "grok", completion_text),
        total_tokens: estimate_tokens(PROVIDER_NAME, "grok", prompt_text)
            + estimate_tokens(PROVIDER_NAME, "grok", completion_text),
    }
}

/// Send a request through the wreq stealth client and return a streaming
/// response, retrying transient 5xx/429 statuses. The Chrome TLS emulation is
/// what clears the Cloudflare Enterprise gate on grok.com; a plain rustls
/// reqwest fingerprint is answered with an anti-bot 403 even with valid
/// sso + sso-rw cookies (same pattern as the Mistral provider).
async fn send_stealth_stream(
    stealth: &obscura_net::StealthHttpClient,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: &str,
) -> Result<obscura_net::StreamingResponse, GatewayError> {
    let parsed_url = url::Url::parse(url)
        .map_err(|e| GatewayError::Internal(format!("invalid URL {url}: {e}")))?;

    for attempt in 1..=3 {
        match stealth
            .send_single_streaming(method, &parsed_url, headers, body)
            .await
        {
            Ok(resp) => {
                let code = resp.status;
                if code >= 500 || code == 429 {
                    if attempt < 3 {
                        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                        continue;
                    }
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                    continue;
                }
                return Err(GatewayError::Internal(format!("stealth request failed: {e}")));
            }
        }
    }
    Err(GatewayError::Internal(
        "stealth request exhausted retries".to_string(),
    ))
}

/// Extract the Sentry release value from grok.com HTML, preserving the
/// URL-encoded form the browser sends in Baggage
/// (`grok-web%40<40-hex build hash>` — capture-verified,
/// captures/WhileCapturingGrok).
fn extract_sentry_release(html: &str) -> Option<String> {
    let marker = "sentry-release=";
    let pos = html.find(marker)?;
    let rest = &html[pos + marker.len()..];
    let end = rest
        .find(|c: char| c == ',' || c == '"' || c == '\'' || c == '<' || c.is_whitespace())
        .unwrap_or(rest.len());
    let raw = &rest[..end];
    // Must look like grok-web%40<hash>; a bare hex token here would be some
    // other identifier.
    (raw.starts_with("grok-web%40") && raw.len() >= 24).then(|| raw.to_string())
}

/// Fallback: reconstruct from a bare Next.js build id found in the page
/// (40-42 hex chars; same value the release wraps).
fn extract_build_token(html: &str) -> Option<String> {
    for window in html.split('"') {
        let n = window.len();
        if (40..=42).contains(&n) && window.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(format!("grok-web%40{window}"));
        }
    }
    None
}

/// Parse a full NDJSON response body into (text, reasoning, finish_reason).
pub fn parse_ndjson_body(body: &str) -> (String, String, String) {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = "stop".to_string();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Some(ndjson) = parse_ndjson_line(line) {
            use GrokResponseExt;
            if !ndjson.token.is_empty() && !ndjson.is_thinking() {
                full_text.push_str(&ndjson.token);
            }
            if ndjson.is_thinking() && !ndjson.token.is_empty() {
                reasoning.push_str(&ndjson.token);
            }
            if ndjson.is_soft_stop() {
                finish_reason = "stop".to_string();
            }
        }
    }
    (full_text, reasoning, finish_reason)
}

pub struct DirectClient {
    stealth: Arc<obscura_net::StealthHttpClient>,
    mode_id: String,
    store: GrokSessionStore,
    upload_cache: UploadCache,
}

impl DirectClient {
    pub async fn new(
        session: SessionHandle,
        model_id: &str,
        store: GrokSessionStore,
    ) -> Result<Self, GatewayError> {
        // Validate the logged-in session; the stealth client reads cookies
        // from the filtered jar on every request.
        validate_grok_session(&extract_grok_cookies(&session))?;

        // Stealth HTTP client with ALL grok.com cookies and Firefox TLS
        // emulation — matches the real browser that earned these cookies.
        let stealth = Arc::new(obscura_net::StealthHttpClient::with_emulation(
            super::auth::filtered_grok_jar(&session),
            None,
            Some(obscura_net::Timeouts::streaming()),
            wreq_util::Profile::Firefox133,
            wreq_util::Platform::Linux,
        ));

        Ok(Self {
            stealth,
            mode_id: mode_id_for_wire(model_id).to_string(),
            store,
            upload_cache: UploadCache::new(),
        })
    }

    /// Headers matching the live grok.com web client (OmniRoute capture).
    /// `x-statsig-id` is a fresh per-request browser error marker; cookies are
    /// applied by the stealth client from the shared jar, so no Cookie header
    /// is needed here (the jar already carries sso + sso-rw).
    /// Scrape the current grok.com deploy release from the homepage.
    ///
    /// The page embeds the Sentry build hash the frontend declares in its
    /// Baggage header; upstream rejects requests whose declared release does
    /// not match the live deploy with code 7 ("page out of date").
    async fn current_release(&self) -> Option<String> {
        if let Some(cached) = self.store.cached_release() {
            return Some(cached);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .ok()?;
        let html = client
            .get("https://grok.com/")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
            )
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?;
        // Prefer the explicit sentry-release marker; fall back to a bare
        // 40-hex build token (the shape of grok.com's Next.js build id).
        let release = extract_sentry_release(&html).or_else(|| extract_build_token(&html))?;
        tracing::info!(release = %release, "Grok deploy release scraped");
        self.store.store_release(release.clone());
        Some(release)
    }

    fn build_request_headers_with_release(
        &self,
        release: Option<&str>,
        statsig: Option<&str>,
    ) -> std::collections::HashMap<String, String> {
        let mut headers = Self::build_request_headers_base();
        if let Some(release) = release {
            headers.insert(
                "Baggage".into(),
                format!(
                    "sentry-environment=production,sentry-release={release},sentry-public_key=b311e0f2690c81f25e2c4cf6d4f7ce1c"
                ),
            );
        }
        // Prefer a signed id harvested from the live page; the synthetic
        // error marker is rejected by current deploys (code 7).
        let statsig = statsig
            .map(String::from)
            .or_else(|| self.store.cached_statsig())
            .unwrap_or_else(super::statsig::browser_statsig_id);
        headers.insert("x-statsig-id".into(), statsig);
        headers.insert("x-xai-request-id".into(), new_uuid());
        let trace_id: String = (0..16).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
        let span_id: String = (0..8).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
        headers.insert("traceparent".into(), format!("00-{trace_id}-{span_id}-00"));
        headers
    }

    /// Resolve headers for one request: cached/scraped deploy release and
    /// cached/harvested statsig when available.
    async fn request_headers(&self) -> std::collections::HashMap<String, String> {
        let release = self.current_release().await;
        let statsig = self.store.cached_statsig();
        self.build_request_headers_with_release(release.as_deref(), statsig.as_deref())
    }

    fn build_request_headers_base() -> std::collections::HashMap<String, String> {
        // Match the REAL logged-in Firefox browser byte-for-byte.
        // Previous Chrome/Windows UA + Chrome-only Sec-Ch-Ua headers were a
        // mismatch against Firefox-earned cookies and triggered code 7.
        let mut headers = std::collections::HashMap::new();
        headers.insert("Accept".into(), "*/*".into());
        headers.insert("Accept-Language".into(), "en-US,en;q=0.5".into());
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("Origin".into(), "https://grok.com".into());
        headers.insert("Referer".into(), "https://grok.com/".into());
        headers.insert("Sec-Fetch-Dest".into(), "empty".into());
        headers.insert("Sec-Fetch-Mode".into(), "cors".into());
        headers.insert("Sec-Fetch-Site".into(), "same-origin".into());
        headers.insert(
            "User-Agent".into(),
            "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0".into(),
        );
        headers.insert("Priority".into(), "u=6".into());
        headers.insert("TE".into(), "trailers".into());
        headers
    }

    async fn process_attachments(&self, request: &ChatCompletionRequest) -> Result<Vec<String>, GatewayError> {
        let mut processed = Vec::new();
        let all_urls: Vec<String> = request
            .messages
            .iter()
            .flat_map(|m| m.content.image_urls())
            .chain(request.messages.iter().flat_map(|m| m.content.file_urls()))
            .collect();

        for url in &all_urls {
            if url.starts_with("data:") {
                let (data, mime) = upload::decode_data_uri(url)
                    .ok_or_else(|| GatewayError::BadRequest("invalid data URI".to_string()))?;
                let ext = mime.split('/').last().unwrap_or("bin");
                let filename = format!("upload.{}", ext);
                let uploaded = upload::upload_file(
                    data,
                    &filename,
                    &mime,
                    &self.stealth,
                    &self.upload_cache,
                ).await?;
                processed.push(uploaded);
            } else {
                processed.push(url.clone());
            }
        }

        Ok(processed)
    }

    fn handle_tool_results(&self, conversation_id: &str, messages: &[ChatMessage]) -> String {
        let tool_msgs: Vec<&ChatMessage> = messages.iter().filter(|m| m.role == "tool").collect();
        if tool_msgs.is_empty() {
            return String::new();
        }
        let mut items = Vec::new();
        for msg in tool_msgs {
            let call = match msg.tool_call_id.as_deref() {
                Some(id) => self.store.get_tool_call(conversation_id, id),
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

    pub fn build_conversation_payload(&self, request: &ChatCompletionRequest, processed_urls: &[String], conversation_id: &str) -> serde_json::Value {
        let mut user_message = request
            .messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_text())
            .unwrap_or_default();

        let tool_context = self.handle_tool_results(conversation_id, &request.messages);
        if !tool_context.is_empty() {
            user_message = format!("{}\n\n{}", tool_context, user_message);
        }

        let system_message = request
            .messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.as_text());

        let file_attachments: Vec<serde_json::Value> = processed_urls
            .iter()
            .map(|url| {
                serde_json::json!({
                    "type": "image",
                    "url": url
                })
            })
            .collect();

        let mut payload = serde_json::json!({
            "temporary": true,
            "modeId": self.mode_id,
            "message": user_message,
            "fileAttachments": file_attachments,
            "imageAttachments": [],
            "disableSearch": !request.search.unwrap_or(false),
            "enableImageGeneration": true,
            "returnImageBytes": false,
            "returnRawGrokInXaiRequest": false,
            "enableImageStreaming": true,
            "imageGenerationCount": 2,
            "forceConcise": false,
            "toolOverrides": {},
            "enableSideBySide": true,
            "isPreset": false,
            "sendFinalMetadata": true,
            "customInstructions": system_message.unwrap_or_default(),
            "deepsearchPreset": "",
            "isReasoning": request.thinking.unwrap_or(false),
            "skipResponseCache": true,
            "disableTextFollowUps": false,
            "disableMemory": true,
            "forceSideBySide": false,
            "isAsyncChat": false,
            "disableSelfHarmShortCircuit": false,
            "deviceEnvInfo": {
                "darkModeEnabled": false,
                "devicePixelRatio": 2,
                "screenWidth": 2056,
                "screenHeight": 1329,
                "viewportWidth": 2056,
                "viewportHeight": 1083,
            },
        });

        if let Some(tools) = &request.tools {
            if !tools.is_empty() {
                // grok.com keeps conversation state server-side; we only send
                // the newest instruction plus the dialect prompt (once).
                let mtp_prompt = mtp::build_mtp_system_prompt(tools, request.tool_choice.as_ref(), false);
                payload["message"] = serde_json::json!(mtp::compose_flat_prompt(
                    mtp::FlatPrompt {
                        system: Some(&mtp_prompt),
                        transcript: "",
                        tool_results: "",
                        current_request: &user_message,
                    }
                ));
            }
        }

        payload
    }

    /// Process the byte stream from grok.com and return parsed results.
    /// Returns (text, reasoning, tool_calls, finish_reason) where tool_calls
    /// being Some means the response should end with finish_reason: "tool_calls".
    async fn process_stream(
        response: obscura_net::StreamingResponse,
        tools: &[Tool],
    ) -> Result<(String, String, Option<Vec<ToolCall>>, String), GatewayError> {
        let mut full_text = String::new();
        let mut reasoning_text = String::new();
        let mut finish_reason = "stop".to_string();
        let mut mtp_state = mtp::MtpStreamState::new();

        let mut stream = response.bytes;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| GatewayError::Internal(format!("stream read error: {e}")))?;
            let text = String::from_utf8_lossy(&chunk);

            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let ndjson = match parse_ndjson_line(line) {
                    Some(r) => r,
                    None => continue,
                };

                // Skip internal tool execution results (grokov server handled these)
                if ndjson.message_tag == "raw_function_result" || ndjson.message_tag == "tool_usage_card" {
                    continue;
                }

                // Priority 1: structured native tool card
                if let Some((id, tool_name, args)) = parse_native_tool_card(&ndjson, tools) {
                    let tc = build_tool_call(&id, &tool_name, args);
                    return Ok((full_text, reasoning_text, Some(vec![tc]), "tool_calls".to_string()));
                }

                // Fallback: native tool card as XML in token text
                if ndjson.is_thinking() && parse_xml_tool_card(&ndjson.token).is_some() {
                    if let Some((id, tool_name, args)) = parse_xml_tool_card(&ndjson.token) {
                        if let Some(mapped_name) = tool_for_native_intent(tools, &tool_name) {
                            let tc = build_tool_call(&id, &mapped_name, args);
                            return Ok((full_text, reasoning_text, Some(vec![tc]), "tool_calls".to_string()));
                        }
                    }
                }

                // Priority 2: MTP tool blocks in content tokens (the
                // universal dialect; blocks are absorbed, never leaked).
                if !ndjson.is_thinking() && !ndjson.token.is_empty() {
                    let visible = mtp_state.process_delta(&ndjson.token, tools);
                    if !visible.is_empty() {
                        full_text.push_str(&visible);
                    }
                }

                // Priority 3: reasoning tokens
                if ndjson.is_thinking() {
                    let t = ndjson.token.trim();
                    if !t.is_empty() && t != "Thinking about your request" && !ndjson.token.contains("<xai:tool_usage_card>") {
                        reasoning_text.push_str(&ndjson.token);
                    }
                    if ndjson.is_soft_stop() {
                        finish_reason = "stop".to_string();
                    }
                    continue;
                }

                // Priority 4: soft-stop on non-content tokens
                if ndjson.token.is_empty() && ndjson.is_soft_stop() {
                    finish_reason = "stop".to_string();
                }
            }
        }

        // Flush any pending block and collect validated calls.
        mtp_state.finish(tools);
        if !mtp_state.collected_tool_calls.is_empty() {
            return Ok((
                full_text,
                reasoning_text,
                Some(std::mem::take(&mut mtp_state.collected_tool_calls)),
                "tool_calls".to_string(),
            ));
        }

        Ok((full_text, reasoning_text, None, finish_reason))
    }

    /// Fail fast while a persisted anti-bot quarantine is armed (e.g. right
    /// after a restart following a 403 challenge) instead of re-hammering
    /// grok.com. The error text classifies as `challenge_required`, which
    /// arms the in-memory circuit for its normal cooldown.
    fn ensure_not_quarantined(&self) -> Result<(), GatewayError> {
        if let Some(remaining) = self.store.anti_bot_quarantine_remaining() {
            return Err(GatewayError::Provider(format!(
                "Grok API 403 anti-bot quarantine active for another {}s: solve the \
                 challenge in the source browser, then retry",
                remaining.as_secs()
            )));
        }
        Ok(())
    }

    async fn send_chat_request(
        &self,
        payload: &serde_json::Value,
    ) -> Result<obscura_net::StreamingResponse, GatewayError> {
        let url = format!("{GROK_BASE_URL}{CONVERSATIONS_PATH}");
        let body = serde_json::to_string(payload)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;
        let headers = self.request_headers().await;
        send_stealth_stream(&self.stealth, "POST", &url, &headers, &body).await
    }

    /// Drain up to 2 KB of a streaming response body into a String for error
    /// messages. Consumes the response body.
    async fn drain_error_body(&self, response: &mut obscura_net::StreamingResponse) -> String {
        let mut out = String::new();
        while out.len() < 2000 {
            match response.bytes.next().await {
                Some(Ok(b)) => out.push_str(&String::from_utf8_lossy(&b)),
                _ => break,
            }
        }
        out.chars().take(2000).collect()
    }

    /// Check the response status. On 200-299 return it unchanged. On 403
    /// (anti-bot) retry once with a fresh `x-statsig-id` marker: the browser
    /// error marker is generated per request, so the retry naturally carries
    /// a new one. No browser navigation or constant re-extraction is involved
    /// (the retired 70-byte challenge blob is what caused the 403s).
    async fn check_and_retry(
        &self,
        response: obscura_net::StreamingResponse,
        payload: &serde_json::Value,
    ) -> Result<obscura_net::StreamingResponse, GatewayError> {
        let status = response.status;
        if (200..300).contains(&status) {
            return Ok(response);
        }

        let mut response = response;
        let body = self.drain_error_body(&mut response).await;

        // "Page out of date" (code 7) means the deploy release we declared is
        // stale. Drop the cached release so the next attempt rescrapes, then
        // retry once with the fresh one.
        if body.contains("out of date") || (body.contains("\"code\":7") && status == 400) {
            tracing::warn!("Grok deploy release rejected; rescraping and retrying once");
            self.store.invalidate_release();
            let retry_headers = self.request_headers().await;
            let url = format!("{GROK_BASE_URL}{CONVERSATIONS_PATH}");
            let body_json = serde_json::to_string(payload)
                .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;
            let retry = send_stealth_stream(&self.stealth, "POST", &url, &retry_headers, &body_json).await?;
            if (200..300).contains(&retry.status) {
                return Ok(retry);
            }
            let mut retry = retry;
            let retry_body = self.drain_error_body(&mut retry).await;
            return Err(GatewayError::Provider(format!(
                "Grok API error after release refresh: {retry_body}"
            )));
        }

        if status == 403 {
            tracing::warn!(
                body = %body,
                "Grok API 403 anti-bot; retrying once with a fresh x-statsig-id marker"
            );
            let retry = self.send_chat_request(payload).await?;
            if (200..300).contains(&retry.status) {
                return Ok(retry);
            }
            let mut retry = retry;
            let retry_body = self.drain_error_body(&mut retry).await;
            // Persist the challenge so a restart does not immediately
            // re-hammer grok.com while it is still serving the wall.
            let quarantine_secs = self.store.record_anti_bot_403();
            tracing::warn!(
                quarantine_secs,
                "Grok 403 persisted after marker refresh; arming anti-bot quarantine"
            );
            return Err(GatewayError::Provider(format!(
                "Grok API 403 after marker refresh: {}. The grok.com session may be \
                 expired; re-login to grok.com and re-import the browser profile.",
                retry_body
            )));
        }

        Err(GatewayError::Provider(format!(
            "Grok API error ({status}): {body}"
        )))
    }

    pub async fn chat(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let conversation_id = new_uuid();
        let conv = self.store.get_or_create(request.session_url.as_deref(), &request.model, conversation_id);
        let conversation_id = conv.conversation_id;
        self.ensure_not_quarantined()?;
        let processed_urls = self.process_attachments(&request).await?;
        let payload = self.build_conversation_payload(&request, &processed_urls, &conversation_id);

        let response = self.send_chat_request(&payload).await?;
        let response = self.check_and_retry(response, &payload).await?;

        let tools = request.tools.as_deref().unwrap_or(&[]);
        let (full_text, reasoning_text, tool_calls, finish_reason) =
            Self::process_stream(response, tools).await?;
        if let Some(ref calls) = tool_calls {
            self.store.store_tool_calls(&conversation_id, calls);
        }

        let prompt_text: String = request
            .messages
            .iter()
            .map(|m| m.content.as_text())
            .collect();
        let completion_text = format!("{}{}", full_text, reasoning_text);

        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-{}", &conversation_id[..8]),
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: request.model.clone(),
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: ChatContent::String(full_text),
                    name: None,
                    reasoning_content: if reasoning_text.is_empty() {
                        None
                    } else {
                        Some(reasoning_text)
                    },
                    citations: None,
                    tool_calls: tool_calls.clone(),
                    tool_call_id: None,
                },
                finish_reason,
            }],
            usage: estimate_cost(&prompt_text, &completion_text),
            session_url: Some(format!("{}/chat/{}", GROK_BASE_URL, conversation_id)),
        })
    }

    pub async fn chat_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let conversation_id = new_uuid();
        let conv = self.store.get_or_create(request.session_url.as_deref(), &request.model, conversation_id);
        let conversation_id = conv.conversation_id;
        self.ensure_not_quarantined()?;
        let processed_urls = self.process_attachments(&request).await?;
        let payload = self.build_conversation_payload(&request, &processed_urls, &conversation_id);

        let response = self.send_chat_request(&payload).await?;
        let response = self.check_and_retry(response, &payload).await?;

        let model_id = request.model.clone();
        let tools: Arc<[Tool]> = request.tools.clone().unwrap_or_default().into();
        let (tx, rx) = mpsc::unbounded_channel();
        let rx_stream = UnboundedReceiverStream::new(rx);
        let session_url = Some(format!("{}/chat/{}", GROK_BASE_URL, conversation_id));

        tokio::spawn(async move {
            let mut mtp_state = mtp::MtpStreamState::new();
            let mut sent_first_chunk = false;
            let mut sent_tool_calls = false;
            let mut stream = response.bytes;

            loop {
                let chunk = match stream.next().await {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => {
                        let _ = tx.send(Err(GatewayError::Internal(format!("stream error: {e}"))));
                        break;
                    }
                    None => break,
                };

                let text = String::from_utf8_lossy(&chunk);
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    let ndjson = match parse_ndjson_line(line) {
                        Some(t) => t,
                        None => continue,
                    };

                    if sent_tool_calls {
                        continue;
                    }

                    // Skip internal tool execution results
                    if ndjson.message_tag == "raw_function_result" || ndjson.message_tag == "tool_usage_card" {
                        continue;
                    }

                    // Priority 1: structured native tool card
                    if let Some((id, tool_name, args)) = parse_native_tool_card(&ndjson, tools.as_ref()) {
                        sent_tool_calls = true;
                        if !sent_first_chunk {
                            let _ = tx.send(Ok(ChatCompletionChunk {
                                id: format!("chatcmpl-{}", &conversation_id[..8]),
                                object: "chat.completion.chunk".to_string(),
                                created: current_timestamp(),
                                model: model_id.clone(),
                                choices: vec![ChunkChoice {
                                    index: 0,
                                    delta: ChatMessageDelta {
                                        role: Some("assistant".to_string()),
                                        content: None,
                                        reasoning_content: None,
                                        citations: None,
                                        tool_calls: None,
                                    },
                                    finish_reason: None,
                                }],
                                session_url: session_url.clone(),
                            }));
                        }
                        let tc = build_tool_call(&id, &tool_name, args);
                        let _ = tx.send(Ok(ChatCompletionChunk {
                            id: format!("chatcmpl-{}", &conversation_id[..8]),
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
                        break;
                    }

                    // Fallback: native tool card as XML in token text
                    if ndjson.is_thinking() {
                        if let Some((id, tool_name, args)) = parse_xml_tool_card(&ndjson.token) {
                            if let Some(mapped_name) = tool_for_native_intent(tools.as_ref(), &tool_name) {
                                sent_tool_calls = true;
                                if !sent_first_chunk {
                                    let _ = tx.send(Ok(ChatCompletionChunk {
                                        id: format!("chatcmpl-{}", &conversation_id[..8]),
                                        object: "chat.completion.chunk".to_string(),
                                        created: current_timestamp(),
                                        model: model_id.clone(),
                                        choices: vec![ChunkChoice {
                                            index: 0,
                                            delta: ChatMessageDelta {
                                                role: Some("assistant".to_string()),
                                                content: None,
                                                reasoning_content: None,
                                                citations: None,
                                                tool_calls: None,
                                            },
                                            finish_reason: None,
                                        }],
                                        session_url: session_url.clone(),
                                    }));
                                }
                                let tc = build_tool_call(&id, &mapped_name, args);
                                let _ = tx.send(Ok(ChatCompletionChunk {
                                    id: format!("chatcmpl-{}", &conversation_id[..8]),
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
                                break;
                            }
                        }
                        // Skip thinking tokens containing unreachable tool card XML
                        if ndjson.token.contains("<xai:tool_usage_card>") {
                            continue;
                        }
                        // Reasoning tokens
                        if ndjson.token.is_empty() || ndjson.token.trim() == "Thinking about your request" {
                            continue;
                        }
                        let _ = tx.send(Ok(ChatCompletionChunk {
                            id: format!("chatcmpl-{}", &conversation_id[..8]),
                            object: "chat.completion.chunk".to_string(),
                            created: current_timestamp(),
                            model: model_id.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChatMessageDelta {
                                    role: None,
                                    content: None,
                                    reasoning_content: Some(ndjson.token.clone()),
                                    citations: None,
                                    tool_calls: None,
                                },
                                finish_reason: None,
                            }],
                            session_url: session_url.clone(),
                        }));
                        continue;
                    }

                    // Content tokens: feed through the MTP stream state when
                    // tools are in flight so blocks are absorbed, validated
                    // against the client definitions, and never leaked.
                    let mut visible_text: Option<String> = None;
                    if !ndjson.is_thinking() && !ndjson.token.is_empty() {
                        if tools.is_empty() {
                            visible_text = Some(ndjson.token.clone());
                        } else {
                            let visible = mtp_state.process_delta(&ndjson.token, &tools);
                            if !mtp_state.collected_tool_calls.is_empty() {
                                // Defer emission to the post-loop flush below.
                                continue;
                            }
                            if !visible.is_empty() {
                                visible_text = Some(visible);
                            }
                        }
                    }

                    // Normal content tokens
                    if visible_text.is_none() && !(ndjson.token.is_empty() && ndjson.is_soft_stop()) {
                        continue;
                    }

                    let role = if !sent_first_chunk && visible_text.is_some() {
                        sent_first_chunk = true;
                        Some("assistant".to_string())
                    } else {
                        None
                    };

                    let finish_reason = if ndjson.is_soft_stop() {
                        Some("stop".to_string())
                    } else {
                        None
                    };

                    let _ = tx.send(Ok(ChatCompletionChunk {
                        id: format!("chatcmpl-{}", &conversation_id[..8]),
                        object: "chat.completion.chunk".to_string(),
                        created: current_timestamp(),
                        model: model_id.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: ChatMessageDelta {
                                role,
                                content: visible_text,
                                reasoning_content: None,
                                citations: None,
                                tool_calls: None,
                            },
                            finish_reason,
                        }],
                        session_url: session_url.clone(),
                    }));
                }

                if sent_tool_calls {
                    break;
                }
            }

            // Flush any pending MTP block and emit collected tool calls.
            if !tools.is_empty() {
                mtp_state.finish(&tools);
            }
        if !mtp_state.collected_tool_calls.is_empty() && !sent_tool_calls {
            let tc = std::mem::take(&mut mtp_state.collected_tool_calls);
            if !tc.is_empty() {
                    sent_tool_calls = true;
                    if !sent_first_chunk {
                        let _ = tx.send(Ok(ChatCompletionChunk {
                            id: format!("chatcmpl-{}", &conversation_id[..8]),
                            object: "chat.completion.chunk".to_string(),
                            created: current_timestamp(),
                            model: model_id.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChatMessageDelta {
                                    role: Some("assistant".to_string()),
                                    content: None,
                                    reasoning_content: None,
                                    citations: None,
                                    tool_calls: None,
                                },
                                finish_reason: None,
                            }],
                            session_url: session_url.clone(),
                        }));
                    }
                    let _ = tx.send(Ok(ChatCompletionChunk {
                        id: format!("chatcmpl-{}", &conversation_id[..8]),
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
                                tool_calls: Some(tc),
                            },
                            finish_reason: None,
                        }],
                        session_url: session_url.clone(),
                    }));
            }
        }

        let final_finish = if sent_tool_calls {            "tool_calls"
        } else {
            "stop"
        };
        let _ = tx.send(Ok(ChatCompletionChunk {
            id: format!("chatcmpl-{}", &conversation_id[..8]),
            object: "chat.completion.chunk".to_string(),
            created: current_timestamp(),
            model: model_id.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChatMessageDelta::default(),
                finish_reason: Some(final_finish.to_string()),
            }],
            session_url: session_url.clone(),
        }));
        });

        Ok(rx_stream.boxed())
    }
}

pub(crate) trait GrokResponseExt {
    fn is_thinking(&self) -> bool;
    fn is_soft_stop(&self) -> bool;
}

impl GrokResponseExt for GrokStreamResponse {
    fn is_thinking(&self) -> bool {
        self.is_thinking.unwrap_or(false)
    }
    fn is_soft_stop(&self) -> bool {
        self.is_soft_stop.unwrap_or(false)
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;

    #[test]
    fn extracts_url_encoded_release() {
        // Capture-verified shape (captures/WhileCapturingGrok line 892).
        let html = r#"sentry-trace" ... "sentry-environment=production,sentry-release=grok-web%4040480cda06c2c96e3460c999ebbad66c144d2b47f7,sentry-public_key=b311"#;
        let rel = extract_sentry_release(html).unwrap();
        assert_eq!(rel, "grok-web%4040480cda06c2c96e3460c999ebbad66c144d2b47f7");
    }

    #[test]
    fn falls_back_to_build_token_with_prefix() {
        let html = r#"<script src="/_next/static/x.js">"buildId":"40480cda06c2c96e3460c999ebbad66c144d2b47f7""#;
        let rel = extract_build_token(html).unwrap();
        assert!(rel.starts_with("grok-web%40"));
        assert!(rel.ends_with("47f7"));
    }

    #[test]
    fn rejects_bare_hex_as_release() {
        // A bare hash after the marker is NOT a release (missing grok-web@).
        assert!(extract_sentry_release("sentry-release=40480cda06c2c96e3460c999ebbad66c144d2b47f7,").is_none());
    }
}
