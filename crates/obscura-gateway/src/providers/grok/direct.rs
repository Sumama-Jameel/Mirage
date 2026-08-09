use futures::stream::{BoxStream, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, COOKIE};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatContent, ChatMessage,
    ChatMessageDelta, ChunkChoice, FunctionCall, Tool, ToolCall, Usage,
};
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::tool_call::{
    convert_xml_tool_calls, inject_tool_prompt,
};
use crate::providers::send_with_retry;
use crate::session::{SessionHandle, SessionManager};

use std::sync::Arc;

use super::auth::{build_cookie_header, extract_grok_cookies, validate_grok_session};
use super::challenge::ChallengeStore;
use super::state::GrokSessionStore;
use super::statsig::generate_statsig_id;
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

fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Represents a single NDJSON event from grok.com's stream.
/// Mirrors the `result.response` object in grok.com's NDJSON format.
#[derive(Debug, Clone, serde::Deserialize)]
struct GrokStreamResponse {
    #[serde(default)]
    token: String,
    is_thinking: Option<bool>,
    #[serde(rename = "isSoftStop")]
    is_soft_stop: Option<bool>,
    #[serde(default)]
    message_tag: String,
    #[serde(default)]
    tool_usage_card_id: String,
    #[serde(default)]
    tool_usage_card: Option<serde_json::Value>,
    #[serde(default)]
    response_id: String,
    #[serde(default)]
    model_response: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct GrokStreamEvent {
    result: Option<GrokStreamEventResult>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct GrokStreamEventResult {
    response: Option<GrokStreamResponse>,
}

fn parse_ndjson_line(line: &str) -> Option<GrokStreamResponse> {
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

pub struct DirectClient {
    http: reqwest::Client,
    stealth: Option<Arc<obscura_net::StealthHttpClient>>,
    session: SessionHandle,
    sessions: SessionManager,
    mode_id: String,
    store: GrokSessionStore,
    challenge_store: ChallengeStore,
    grok_cookies: Vec<obscura_net::CookieInfo>,
    upload_cache: UploadCache,
}

impl DirectClient {
    pub async fn new(
        session: SessionHandle,
        sessions: &SessionManager,
        model_id: &str,
        store: GrokSessionStore,
        challenge_store: ChallengeStore,
    ) -> Result<Self, GatewayError> {
        let grok_cookies = extract_grok_cookies(&session);
        validate_grok_session(&grok_cookies)?;

        let http = Self::build_http_client()?;

        let stealth = obscura_net::StealthHttpClient::new(session.cookie_jar.clone()).into();

        Ok(Self {
            http,
            stealth: Some(stealth),
            session,
            sessions: sessions.clone(),
            mode_id: mode_id_for_wire(model_id).to_string(),
            store,
            challenge_store,
            grok_cookies,
            upload_cache: UploadCache::new(),
        })
    }

    fn build_http_client() -> Result<reqwest::Client, GatewayError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "User-Agent",
            HeaderValue::from_static(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
            ),
        );
        headers.insert("Accept", HeaderValue::from_static("*/*"));
        headers.insert("Accept-Language", HeaderValue::from_static("en-US,en;q=0.9"));
        headers.insert("Origin", HeaderValue::from_static("https://grok.com"));
        headers.insert("Referer", HeaderValue::from_static("https://grok.com/"));
        headers.insert("Sec-Fetch-Site", HeaderValue::from_static("same-origin"));
        headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;

        Ok(client)
    }

    fn build_request_headers(&self, method: &str, path: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();

        let cookie_value = build_cookie_header(&self.grok_cookies);
        headers.insert(COOKIE, HeaderValue::from_str(&cookie_value).unwrap());

        let config = self.challenge_store.get_config();
        let statsig_id = generate_statsig_id(&config, method, path);
        headers.insert("x-statsig-id", HeaderValue::from_str(&statsig_id).unwrap());

        headers.insert("x-xai-request-id", HeaderValue::from_str(&new_uuid()).unwrap());
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
                    &self.grok_cookies,
                    &self.challenge_store.get_config(),
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
        crate::providers::tool_call::format_tool_results(&refs)
    }

    fn build_conversation_payload(&self, request: &ChatCompletionRequest, processed_urls: &[String], conversation_id: &str) -> serde_json::Value {
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
        });

        if let Some(tools) = &request.tools {
            if !tools.is_empty() {
                let tool_block = inject_tool_prompt(&user_message, tools, request.tool_choice.as_ref());
                payload["message"] =
                    serde_json::json!(format!("{}\n\n{}", user_message, tool_block));
            }
        }

        payload
    }

    /// Process the byte stream from grok.com and return parsed results.
    /// Returns (text, reasoning, tool_calls, finish_reason) where tool_calls
    /// being Some means the response should end with finish_reason: "tool_calls".
    async fn process_stream(
        response: reqwest::Response,
        tools: &[Tool],
    ) -> Result<(String, String, Option<Vec<ToolCall>>, String), GatewayError> {
        let mut full_text = String::new();
        let mut reasoning_text = String::new();
        let mut finish_reason = "stop".to_string();
        let mut xml_tool_content = String::new();
        let mut is_collecting_xml_tool = false;
        let mut sent_tool_calls = false;

        let mut stream = Box::pin(response.bytes_stream());

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
                    if !sent_tool_calls {
                        sent_tool_calls = true;
                        let tc = build_tool_call(&id, &tool_name, args);
                        return Ok((full_text, reasoning_text, Some(vec![tc]), "tool_calls".to_string()));
                    }
                }

                // Fallback: native tool card as XML in token text
                if ndjson.is_thinking() && parse_xml_tool_card(&ndjson.token).is_some()
                    && !sent_tool_calls
                {
                    if let Some((id, tool_name, args)) = parse_xml_tool_card(&ndjson.token) {
                        if let Some(mapped_name) = tool_for_native_intent(tools, &tool_name) {
                            sent_tool_calls = true;
                            let tc = build_tool_call(&id, &mapped_name, args);
                            return Ok((full_text, reasoning_text, Some(vec![tc]), "tool_calls".to_string()));
                        }
                    }
                }

                // Priority 2: XML tool call (prompt injection via <tool_call>)
                if ndjson.token.contains("<tool_call>") {
                    is_collecting_xml_tool = true;
                }
                if is_collecting_xml_tool {
                    xml_tool_content.push_str(&ndjson.token);
                    continue;
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

                // Priority 4: content tokens
                if !ndjson.token.is_empty() || ndjson.is_soft_stop() {
                    full_text.push_str(&ndjson.token);
                }
                if ndjson.is_soft_stop() {
                    finish_reason = "stop".to_string();
                }
            }
        }

        // After stream ends, check for XML tool calls
        if !xml_tool_content.is_empty() && !tools.is_empty() {
            let (remaining_text, maybe_calls) = convert_xml_tool_calls(&xml_tool_content, true);
            if let Some(tc) = maybe_calls {
                if !tc.is_empty() {
                    return Ok((remaining_text, reasoning_text, Some(tc), "tool_calls".to_string()));
                }
            }
        }

        Ok((full_text, reasoning_text, None, finish_reason))
    }

    fn is_403_error(response: &reqwest::Response) -> bool {
        response.status().as_u16() == 403
    }

    async fn send_chat_request(
        &self,
        payload: &serde_json::Value,
    ) -> Result<reqwest::Response, GatewayError> {
        let url_str = format!("{}{}", GROK_BASE_URL, CONVERSATIONS_PATH);
        let headers = self.build_request_headers("POST", CONVERSATIONS_PATH);

        // Diagnostic: try stealth client and log result
        // self.diagnose_stealth(&url_str, &headers, payload).await;

        let builder = self
            .http
            .post(&url_str)
            .headers(headers)
            .json(payload);
        send_with_retry(builder).await
    }

    /*
    /// Diagnostic: send one request via the stealth client and log the status/body.
    /// This never changes the real request path.
    async fn diagnose_stealth(
        &self,
        url: &str,
        headers: &reqwest::header::HeaderMap,
        payload: &serde_json::Value,
    ) {
        tracing::warn!("[stealth-diagnose] Entering diagnose_stealth");
        let stealth = match self.stealth.as_ref() {
            Some(s) => s,
            None => {
                tracing::warn!("[stealth-diagnose] No stealth client available");
                return;
            }
        };
        let parsed_url: url::Url = match url.parse() {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("[stealth-diagnose] invalid url: {e}");
                return;
            }
        };
        let mut h = HashMap::new();
        for (k, v) in headers.iter() {
            if let Ok(v) = v.to_str() {
                h.insert(k.to_string(), v.to_string());
            }
        }
        let body_str = match serde_json::to_string(payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("[stealth-diagnose] serialize: {e}");
                return;
            }
        };
        match stealth.send_single("POST", &parsed_url, &h, &body_str).await {
            Ok(resp) => {
                let preview = String::from_utf8_lossy(&resp.body[..resp.body.len().min(200)]);
                tracing::warn!(
                    "[stealth-diagnose] status={} body={}",
                    resp.status,
                    preview,
                );
            }
            Err(e) => {
                tracing::warn!("[stealth-diagnose] error: {e}");
            }
        }
        tracing::warn!("[stealth-diagnose] Exiting (completed)");
    }
    */

    fn is_grok_403_status(status: u16, body: &str) -> bool {
        if status == 403 {
            tracing::warn!(
                body = %body,
                "Grok API 403 — challenge constants may be stale"
            );
            true
        } else {
            false
        }
    }

    async fn try_heal_challenge(&self) -> Result<bool, GatewayError> {
        tracing::warn!("Attempting to auto-heal Grok challenge constants");
        match self
            .challenge_store
            .renew(&self.sessions, &self.session.id)
            .await
        {
            Ok(()) => {
                tracing::info!("Grok challenge constants healed; retrying request");
                Ok(true)
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to heal Grok challenge constants");
                Ok(false)
            }
        }
    }

    /// Check response status and return Ok(response) on success,
    /// or Err with optional auto-heal of challenge constants on 403.
    async fn check_and_heal(
        &self,
        response: reqwest::Response,
        payload: &serde_json::Value,
    ) -> Result<reqwest::Response, GatewayError> {
        let status = response.status().as_u16();
        if status == 200 {
            return Ok(response);
        }

        let body = response.text().await.unwrap_or_default();

        if Self::is_grok_403_status(status, &body) {
            if self.try_heal_challenge().await? {
                let retry = self.send_chat_request(payload).await?;
                let retry_status = retry.status().as_u16();
                if retry_status != 200 {
                    let retry_body = retry.text().await.unwrap_or_default();
                    if Self::is_grok_403_status(retry_status, &retry_body) {
                        return Err(GatewayError::Provider(format!(
                            "Grok API 403 after challenge refresh: {}. \
                             Your grok.com login session may have expired. \
                             Re-login to grok.com and re-run.",
                            retry_body
                        )));
                    }
                    return Err(GatewayError::Provider(format!(
                        "Grok API error after retry ({}): {}",
                        retry_status, retry_body
                    )));
                }
                return Ok(retry);
            }
            return Err(GatewayError::Provider(format!(
                "Grok API 403 Forbidden: {}. \
                 Auto-heal failed — the x-statsig-id constants could not \
                 be refreshed. Ensure grok.com is accessible and the \
                 browser session is logged in.",
                body
            )));
        }

        Err(GatewayError::Provider(format!(
            "Grok API error ({}): {}",
            status, body
        )))
    }

    pub async fn chat(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let conversation_id = new_uuid();
        let conv = self.store.get_or_create(request.session_url.as_deref(), &request.model, conversation_id);
        let conversation_id = conv.conversation_id;
        let processed_urls = self.process_attachments(&request).await?;
        let payload = self.build_conversation_payload(&request, &processed_urls, &conversation_id);

        let response = self.send_chat_request(&payload).await?;
        let response = self.check_and_heal(response, &payload).await?;

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
        let processed_urls = self.process_attachments(&request).await?;
        let payload = self.build_conversation_payload(&request, &processed_urls, &conversation_id);

        let response = self.send_chat_request(&payload).await?;
        let response = self.check_and_heal(response, &payload).await?;

        let model_id = request.model.clone();
        let tools: Arc<[Tool]> = request.tools.clone().unwrap_or_default().into();
        let (tx, rx) = mpsc::unbounded_channel();
        let rx_stream = UnboundedReceiverStream::new(rx);
        let session_url = Some(format!("{}/chat/{}", GROK_BASE_URL, conversation_id));

        tokio::spawn(async move {
            let mut xml_tool_content = String::new();
            let mut is_collecting_xml_tool = false;
            let mut sent_first_chunk = false;
            let mut sent_tool_calls = false;
            let mut stream = Box::pin(response.bytes_stream());

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

                    // XML tool call (prompt injection)
                    if ndjson.token.contains("<tool_call>") {
                        is_collecting_xml_tool = true;
                    }
                    if is_collecting_xml_tool {
                        xml_tool_content.push_str(&ndjson.token);
                        continue;
                    }

                    // Normal content tokens
                    if ndjson.token.is_empty() && !ndjson.is_soft_stop() {
                        continue;
                    }

                    let role = if !sent_first_chunk {
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
                                content: if ndjson.token.is_empty() { None } else { Some(ndjson.token.clone()) },
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

            // After stream ends, check for collected XML tool calls
        if !xml_tool_content.is_empty() && !tools.as_ref().is_empty() && !sent_tool_calls {
            let (_remaining, maybe_calls) = convert_xml_tool_calls(&xml_tool_content, true);
            if let Some(tc) = maybe_calls {
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
        }

        let final_finish = if sent_tool_calls {
            "tool_calls"
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

trait GrokResponseExt {
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
