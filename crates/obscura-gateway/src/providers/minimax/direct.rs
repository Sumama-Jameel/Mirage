use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Local;
use futures::stream::{BoxStream, StreamExt};
use md5::{Digest, Md5};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatContent,
    ChatMessage, ChatMessageDelta, ChunkChoice, FunctionCall, ToolCall, Usage,
};
use crate::session::SessionHandle;

use super::state::{MinimaxSessionState, MinimaxSessionStore};

const API_HOST: &str = "https://agent.minimax.io";
const STREAM_HOST: &str = "https://agent-stream.minimax.io";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.12) Gecko/20100101 Firefox/140.12";
const AGENT_BASE: &str = "/archon/api/v1";
const MAVIS_AGENT_ID: &str = "405561744400588";

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Matches JS `encodeURIComponent`: spaces → `%20`, safe = `A-Za-z0-9-._~`.
/// Used for the yy computation (double-encoding step).
fn urlencode(s: &str) -> String {
    let mut result = String::new();
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

/// Matches JS `URLSearchParams.toString()` / Python `urllib.parse.urlencode`:
/// spaces → `+`, per `application/x-www-form-urlencoded`.
/// Used for building the query string (`push_param`).
fn urlencode_form(s: &str) -> String {
    let mut result = String::new();
    for &byte in s.as_bytes() {
        match byte {
            b' ' => result.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

fn parse_jwt(jwt: &str) -> Result<(String, String), GatewayError> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return Err(GatewayError::BadRequest("invalid JWT format".into()));
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| GatewayError::BadRequest(format!("invalid JWT payload: {e}")))?;
    let json: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|e| GatewayError::BadRequest(format!("invalid JWT payload JSON: {e}")))?;

    let user_id = json
        .get("sub")
        .and_then(|v| v.as_str())
        .or_else(|| {
            json.get("user")
                .and_then(|u| u.get("id"))
                .and_then(|v| v.as_str())
        })
        .ok_or_else(|| {
            GatewayError::BadRequest(
                "JWT missing user id (expected 'sub' or 'user.id')".into(),
            )
        })?
        .to_string();

    let device_id = json
        .get("sub")
        .and_then(|v| v.as_str())
        .and_then(|s| s.splitn(2, '_').nth(1))
        .map(|s| s.to_string())
        .or_else(|| {
            json.get("user")
                .and_then(|u| u.get("deviceID"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    Ok((user_id, device_id))
}

/// Build query params matching the actual browser request — JS `mC()` + `L()` with
/// all runtime params (cpu_core_num, browser_language, browser_platform, screen).
///
/// The browser sends `client=web&region=en` in the actual URL (not a separate yy path),
/// so this function produces THE SAME string for both yy computation and the HTTP request.
///
/// Order and param logic matches the real browser's `sign_request()` output.
fn build_query_string(
    jwt: &str,
    uuid: &str,
    device_id: &str,
    user_id: &str,
    unix_ms_val: i64,
    timezone_offset_secs: i64,
) -> String {
    let mut out = String::with_capacity(512);

    push_param(&mut out, "device_platform", "web");
    push_param(&mut out, "biz_id", "3");
    push_param(&mut out, "app_id", "3001");
    push_param(&mut out, "version_code", "22201");
    push_param(&mut out, "unix", &unix_ms_val.to_string());
    push_param(&mut out, "timezone_offset", &timezone_offset_secs.to_string());
    push_param(&mut out, "sys_language", "en");
    push_param(&mut out, "lang", "en");

    if !uuid.is_empty() {
        push_param(&mut out, "uuid", uuid);
    }
    if !device_id.is_empty() {
        push_param(&mut out, "device_id", device_id);
    }
    push_param(&mut out, "os_name", "Windows");
    push_param(&mut out, "browser_name", "Chrome");
    // cpu_core_num, browser_language, browser_platform only when non-empty
    // (the Python reference omits them when zero/empty)
    if !user_id.is_empty() {
        push_param(&mut out, "user_id", user_id);
    }
    // op_ticket is deliberately not sent — the Python reference does not
    // include it in the URL or yy computation.
    push_param(&mut out, "screen_width", "1366");
    push_param(&mut out, "screen_height", "768");
    push_param(&mut out, "token", jwt);
    push_param(&mut out, "client", "web");
    push_param(&mut out, "region", "en");

    out
}

fn push_param(out: &mut String, key: &str, value: &str) {
    if !out.is_empty() {
        out.push('&');
    }
    out.push_str(&urlencode_form(key));
    out.push('=');
    out.push_str(&urlencode_form(value));
}

fn build_uri(path: &str, query: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1 + query.len());
    out.push_str(path);
    out.push('?');
    out.push_str(query);
    out
}

/// Signing secret extracted from the MiniMax web app JS bundle.
///
/// This secret is used to compute the `yy` header for request signing.
/// It was reverse-engineered from the official MiniMax web interface JS module #52724.
///
/// **Environment Override**: Set `MINIMAX_SIGNING_SECRET` to use a custom secret.
/// This allows updating the secret without recompiling when MiniMax updates their frontend.
/// Example: `export MINIMAX_SIGNING_SECRET="new_secret_value"`
///
/// When the env var is not set, uses the hardcoded default below.
/// The secret should be ~25 characters and contain special characters.
fn get_signing_secret() -> String {
    std::env::var("MINIMAX_SIGNING_SECRET").unwrap_or_else(|_| {
        "I*7Cf%WZ#S&%1RlZJ&C2".to_string()
    })
}

/// Compute the yy header value matching JS module 52724 exactly.
///
/// Algorithm:
///   body_for_yy = JSON.stringify(JSON.parse(body_str))  // canonical re-serialize
///                 or "{}" for empty/non-POST
///   inner = encodeURIComponent(hasSearchParamsPath)
///           + "_" + body_for_yy
///           + MD5(String(timestamp_ms))
///           + "ooui"
///   yy = MD5(inner)
fn compute_yy(uri_with_query: &str, body: &str, unix_ms_val: i64) -> String {
    let body_for_yy = if body.is_empty() || body == "{}" {
        "{}".to_string()
    } else {
        // Parse and canonical re-serialize (json.dumps(obj, separators=(",", ":")))
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(val) => serde_json::to_string(&val).unwrap_or_else(|_| body.to_string()),
            Err(_) => body.to_string(),
        }
    };
    let ts_ms = unix_ms_val.to_string();
    let uri_encoded = urlencode(uri_with_query);
    let ms_md5 = md5_hex(&ts_ms);
    let inner = format!("{}_{}{}ooui", uri_encoded, body_for_yy, ms_md5);
    md5_hex(&inner)
}

fn build_auth_headers(
    jwt: &str,
    body: &str,
    uri_with_query: &str,
    unix_ms_val: i64,
) -> HashMap<String, String> {
    let ts_sec = (unix_ms_val / 1000).to_string();

    let signing_secret = get_signing_secret();
    let sig_input = format!("{}{}{}", ts_sec, signing_secret, body);
    let signature = md5_hex(&sig_input);

    let yy = compute_yy(uri_with_query, body, unix_ms_val);

    let mut h = HashMap::with_capacity(5);
    h.insert("content-type".to_string(), "application/json".to_string());
    h.insert("token".to_string(), jwt.to_string());
    h.insert("x-timestamp".to_string(), ts_sec);
    h.insert("x-signature".to_string(), signature);
    h.insert("yy".to_string(), yy);
    h
}

/// Map user-facing model names to agent IDs.
fn model_to_agent_id(public_model: &str) -> &'static str {
    match public_model {
        "minimax-m3" | "minimax-m2.7" | "minimax-m2.7-highspeed" => MAVIS_AGENT_ID,
        _ => MAVIS_AGENT_ID,
    }
}

fn upstream_model(public_model: &str) -> &'static str {
    match public_model {
        "minimax-m3" => "MiniMax-M3",
        "minimax-m2.7" => "MiniMax-M2.7",
        "minimax-m2.7-highspeed" => "MiniMax-M2.7-highspeed",
        _ => "MiniMax-M3",
    }
}

/// Map the gateway model + per-request `thinking` toggle to the archon
/// `model.variant` field. The web app sends `variant: ""` for the default/off
/// state and `"thinking"` when M3 thinking is enabled (docs/MINIMAX_ARCHON_CONSTANTS.txt).
/// M3 thinking is switchable; M2.7 reasoning is forced on, so the variant only
/// ever selects "thinking" there (validate_request rejects `thinking: false`
/// for M2.7 models before this point).
fn variant_for_model(public_model: &str, thinking: Option<bool>) -> &'static str {
    match thinking {
        Some(false) if public_model == "minimax-m3" => "",
        Some(true) => "thinking",
        _ => "thinking",
    }
}

fn find_sse_boundary(buf: &[u8]) -> Option<usize> {
    let len = buf.len();
    if len < 2 {
        return None;
    }
    if len >= 4 {
        let mut i = 0;
        while i <= len - 4 {
            if buf[i] == b'\r' && buf[i + 1] == b'\n' && buf[i + 2] == b'\r' && buf[i + 3] == b'\n'
            {
                return Some(i + 4);
            }
            i += 1;
        }
    }
    let mut i = 0;
    while i <= len - 2 {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

fn parse_sse_line(line: &str) -> Option<&str> {
    let line = line.trim();
    line.strip_prefix("data: ")
        .or_else(|| line.strip_prefix("data:"))
        .map(|s| s.trim())
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SseEvent {
    #[serde(rename = "type")]
    event_type: i64,
    #[serde(default, alias = "agentMessageChunk")]
    agent_message_chunk: Option<AgentMessageChunk>,
    #[serde(default, alias = "agentMessage")]
    agent_message: Option<AgentMessage>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct AgentMessageChunk {
    #[serde(default, alias = "thinkingContent")]
    thinking_content: Option<String>,
    #[serde(default, alias = "msgContent")]
    msg_content: Option<String>,
    #[serde(default)]
    finish: Option<bool>,
    #[serde(default, alias = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AgentMessage {
    #[serde(default)]
    msg_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    msg_type: Option<serde_json::Value>,
    #[serde(default, alias = "msgContent")]
    msg_content: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default, alias = "finishReason")]
    finish_reason: Option<String>,
    #[serde(default)]
    tool_calls: Option<serde_json::Value>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

#[derive(Default)]
struct StreamState {
    mtp_state: crate::providers::mtp::MtpStreamState,
    tool_defs: Vec<crate::models::Tool>,
    collected_tool_calls: Vec<ToolCall>,
}

/// Parse native tool_calls from the `agent_message` type:2 event.
/// Supports OpenAI-format arrays and Minimax-native formats.
fn parse_native_tool_calls(value: &serde_json::Value) -> Result<Vec<ToolCall>, String> {
    let calls = value.as_array().ok_or_else(|| "tool_calls not an array".to_string())?;
    let mut result = Vec::new();
    for (i, call) in calls.iter().enumerate() {
        let call_type = call.get("type").and_then(|t| t.as_str()).unwrap_or("function");
        if call_type != "function" {
            tracing::debug!("Skipping non-function tool call type={call_type}");
            continue;
        }
        let id = call.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("call_{i}"));
        let name = call.pointer("/function/name")
            .or_else(|| call.pointer("/functionName"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown_function")
            .to_string();
        let arguments = match call.pointer("/function/arguments")
            .or_else(|| call.pointer("/arguments"))
        {
            Some(v) if v.is_string() => v.as_str().unwrap_or("{}").to_string(),
            Some(v) if v.is_object() => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()),
            _ => "{}".to_string(),
        };
        result.push(ToolCall {
            id,
            r#type: "function".to_string(),
            function: FunctionCall { name, arguments },
        });
    }
    Ok(result)
}

fn parse_minimax_tool_calls(content: &str) -> (String, Option<Vec<ToolCall>>) {
    const START: &str = "<minimax:tool_call>";
    const END: &str = "</minimax:tool_call>";
    let mut output = String::new();
    let mut calls = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        if let Some(start_pos) = rest.find(START) {
            output.push_str(&rest[..start_pos]);
            let after_start = &rest[start_pos + START.len()..];
            if let Some(end_pos) = after_start.find(END) {
                let inner = &after_start[..end_pos];
                for invoke in inner.split("<invoke ") {
                    if invoke.trim().is_empty() {
                        continue;
                    }
                    if let Some(tc) = parse_minimax_invoke(invoke) {
                        calls.push(tc);
                    }
                }
                rest = &after_start[end_pos + END.len()..];
            } else {
                // No closing tag — parse what we have (incomplete XML)
                for invoke in after_start.split("<invoke ") {
                    if invoke.trim().is_empty() {
                        continue;
                    }
                    if let Some(tc) = parse_minimax_invoke(invoke) {
                        calls.push(tc);
                    }
                }
                rest = "";
            }
        } else {
            output.push_str(rest);
            break;
        }
    }
    let result = if calls.is_empty() { None } else { Some(calls) };
    (output, result)
}

fn parse_minimax_invoke(invoke: &str) -> Option<ToolCall> {
    let name = invoke.split("name=\"").nth(1)?.split('"').next()?.to_string();
    let mut params = serde_json::Map::new();
    // Try <parameter name="key">val</parameter> format first
    for param in invoke.split("<parameter ") {
        if param.contains("</invoke>") && param.contains("name=\"") {
            let pname = param.split("name=\"").nth(1)?.split('"').next()?.to_string();
            let pval = param.split('>').nth(1)?.split("</parameter>").next()?.trim().to_string();
            params.insert(pname, serde_json::Value::String(pval));
        }
    }
    // If no params parsed, try direct child element format: <city>London or <city>London</city>
    if params.is_empty() {
        for part in invoke.split('<') {
            if !part.contains('>') { continue; }
            let pname = part.split('>').next()?.trim();
            if pname.is_empty() || pname.starts_with('/') || pname.starts_with('?') { continue; }
            if pname == "invoke" || pname.contains(' ') { continue; }
            let pval = part.split('>').nth(1)?.split('<').next()?.trim();
            if !pval.is_empty() {
                params.insert(pname.to_string(), serde_json::Value::String(pval.to_string()));
            }
        }
    }
    let arguments_str = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());
    Some(ToolCall {
        id: format!("call_{}", uuid::Uuid::new_v4().simple()),
        r#type: "function".to_string(),
        function: crate::models::FunctionCall {
            name,
            arguments: arguments_str,
        },
    })
}

fn process_content_text(content: &str, state: &mut StreamState) -> String {
    // MTP blocks are absorbed, validated against the client definitions,
    // and collected; visible text passes through.
    let out = state.mtp_state.process_delta(content, &state.tool_defs);
    for tc in state.mtp_state.collected_tool_calls.drain(..) {
        state.collected_tool_calls.push(tc);
    }
    out
}

fn build_extra_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("user-agent".to_string(), UA.to_string());
    h
}

async fn send_stealth(
    stealth: &obscura_net::StealthHttpClient,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: &str,
) -> Result<obscura_net::Response, GatewayError> {
    let parsed_url = url::Url::parse(url)
        .map_err(|e| GatewayError::Internal(format!("invalid URL {url}: {e}")))?;

    for attempt in 1..=3 {
        match stealth.send_single(method, &parsed_url, headers, body).await {
            Ok(resp) => {
                let code = resp.status;
                if code >= 500 || code == 429 {
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64))
                            .await;
                        continue;
                    }
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64))
                        .await;
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

fn build_stealth_client(session: &SessionHandle) -> Arc<obscura_net::StealthHttpClient> {
    let stealth = Arc::new(obscura_net::StealthHttpClient::new(session.cookie_jar.clone()));
    let extra = build_extra_headers();
    let s = stealth.clone();
    tokio::spawn(async move {
        s.set_extra_headers(extra).await;
    });
    stealth
}

fn build_reqwest_client() -> Result<reqwest::Client, GatewayError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(UA),
    );
    headers.insert(
        "accept",
        HeaderValue::from_static("text/event-stream, application/json"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| GatewayError::Internal(format!("failed to build reqwest client: {e}")))
}

pub struct DirectClient {
    session: SessionHandle,
    model_id: String,
    jwt: String,
    user_id: String,
    device_id: String,
    uuid: String,
    op_ticket: String,
    store: MinimaxSessionStore,
    upload_cache: super::upload::UploadCache,
}

impl DirectClient {
    pub async fn new(
        session: SessionHandle,
        model_id: &str,
        store: MinimaxSessionStore,
    ) -> Result<Self, GatewayError> {
        let jwt = find_jwt(&session).ok_or_else(|| {
            GatewayError::Auth(
                "Minimax JWT not found. Log in at agent.minimax.io, then either:\n\
                 1. Run Obscura with a browser snapshot that contains the session\n\
                 2. Set the MINIMAX_JWT environment variable to the _token value"
                    .to_string(),
            )
        })?;
        let (_jwt_user_id, mut device_id) = parse_jwt(&jwt)?;

        // Extract UNIQUE_USER_ID from localStorage (persistent UUID from browser).
        let uuid = session
            .local_storage
            .iter()
            .find(|e| e.key == "UNIQUE_USER_ID")
            .map(|e| e.value.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| new_uuid());

        // Parse user_detail_agent for realU (user_id), op_ticket, and device_id.
        let mut user_id = _jwt_user_id.clone();
        let mut op_ticket = String::new();
        if let Some(entry) = session.local_storage.iter().find(|e| e.key == "user_detail_agent") {
            let raw = &entry.value;
            tracing::debug!(
                raw_len = raw.len(),
                raw_preview = %raw.chars().take(200).collect::<String>(),
                "Minimax user_detail_agent found"
            );
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(val) => {
                    if let Some(ru) = val.get("realUserID").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                        user_id = ru.to_string();
                    }
                    if let Some(ot) = val.get("token").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                        op_ticket = ot.to_string();
                    }
                    if op_ticket.is_empty() {
                        if let Some(ot) = val.get("op_ticket").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                            op_ticket = ot.to_string();
                        }
                    }
                    if let Some(did) = val.get("deviceID").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                        device_id = did.to_string();
                    }
                    tracing::info!(
                        user_id = %user_id,
                        has_op_ticket = !op_ticket.is_empty(),
                        has_device_id = !device_id.is_empty(),
                        "Minimax user_detail_agent parsed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        raw_preview = %raw.chars().take(300).collect::<String>(),
                        "Minimax user_detail_agent is not valid JSON — check LSNG decoding"
                    );
                }
            }
        } else {
            tracing::warn!("Minimax user_detail_agent not found in localStorage");
        }

        // If still no device_id, generate a short numeric one (browser uses e.g. '56643951').
        if device_id.is_empty() {
            device_id = format!("{}", (unix_ms() % 100000000).abs());
            tracing::warn!("No device_id found; generated numeric ID");
        }

        Ok(Self {
            session,
            model_id: model_id.to_string(),
            jwt,
            user_id,
            device_id,
            uuid,
            op_ticket,
            store,
            upload_cache: super::upload::UploadCache::new(),
        })
    }

    async fn get_or_create_session(
        &self,
        gateway_session_id: &str,
    ) -> Result<MinimaxSessionState, GatewayError> {
        if let Some(existing) = self.store.next_turn(gateway_session_id).await {
            return Ok(existing);
        }

        let stealth = build_stealth_client(&self.session);
        let agent_id = model_to_agent_id(&self.model_id);

        let ts_ms = unix_ms();
        let timezone_offset_secs = Local::now().offset().local_minus_utc() as i64;
        let path = format!("{}/agent/{}/session", AGENT_BASE, agent_id);

        // Single query string used for BOTH yy computation and the actual URL
        let query = build_query_string(
            &self.jwt,
            &self.uuid,
            &self.device_id,
            &self.user_id,
            ts_ms,
            timezone_offset_secs,
        );
        let uri = build_uri(&path, &query);
        let url = format!("{}{}", API_HOST, uri);

        let upstream = upstream_model(&self.model_id);
        let body_obj = serde_json::json!({
            "team_mode_off": true,
            "model": format!("minimax/{}", upstream),
        });
        let body_str = serde_json::to_string(&body_obj)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;
        let auth_headers = build_auth_headers(&self.jwt, &body_str, &uri, ts_ms);

        tracing::debug!(
            url = %url,
            headers = ?auth_headers,
            body = %body_str,
            "Creating Minimax session"
        );

        let resp = send_stealth(&stealth, "POST", &url, &auth_headers, &body_str).await?;
        let status = resp.status;
        let body = String::from_utf8(resp.body).unwrap_or_default();
        tracing::debug!(status = status, body = %body.chars().take(200).collect::<String>(), resp_headers = ?resp.headers, "Minimax session response");

        if status != 200 {
            return Err(GatewayError::Provider(format!(
                "Minimax create session failed ({}): {}",
                status,
                body.chars().take(300).collect::<String>()
            )));
        }

        let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            GatewayError::Internal(format!(
                "failed to parse session response: {e}, body: {body}"
            ))
        })?;

        let session_id = parsed
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                GatewayError::Internal(format!(
                    "session response missing 'session_id': {body}"
                ))
            })?
            .to_string();

        let state = MinimaxSessionState {
            session_id,
            agent_name: agent_id.to_string(),
            device_id: self.device_id.clone(),
            user_id: self.user_id.clone(),
            uuid: self.uuid.clone(),
            op_ticket: self.op_ticket.clone(),
            model: self.model_id.clone(),
            turn_counter: 1,
            tool_calls: std::collections::HashMap::new(),
        };

        self.store
            .insert(gateway_session_id.to_string(), state.clone())
            .await;

        Ok(state)
    }

    /// Poll the session until it's idle (status.type == 0) before sending a message.
    async fn poll_session_idle(&self, session_id: &str) -> Result<(), GatewayError> {
        let poll_path = format!("{}/session/{}", AGENT_BASE, session_id);
        for _ in 0..5 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let url = format!("{}{}?token={}", API_HOST, poll_path, self.jwt);
            let client = build_reqwest_client()?;
            let resp = client
                .get(&url)
                .header("token", &self.jwt)
                .send()
                .await
                .map_err(|e| {
                    GatewayError::Internal(format!("poll session idle failed: {e}"))
                })?;
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if body
                        .pointer("/session/status/type")
                        .and_then(|v| v.as_i64())
                        == Some(0)
                    {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Non-streaming chat. Buffers the SSE stream internally.
    pub async fn chat(
        &self,
        request: ChatCompletionRequest,
        gateway_session_id: &str,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let response_id = format!("chatcmpl-{}", &new_uuid()[..8]);
        let model = request.model.clone();
        let had_tools = request.tools.is_some();
        let attachments = self.process_attachments(&request).await?;
        let session_state = self.get_or_create_session(gateway_session_id).await?;

        let mut stream = self
            .send_message_sse(&session_state, &request, &attachments, gateway_session_id)
            .await?;

        let mut full_content = String::new();
        let mut full_thinking = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason = "stop".to_string();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for choice in chunk.choices {
                if let Some(ref fr) = choice.finish_reason {
                    finish_reason = fr.clone();
                }
                if let Some(ref c) = choice.delta.content {
                    full_content.push_str(c);
                }
                if let Some(ref t) = choice.delta.reasoning_content {
                    full_thinking.push_str(t);
                }
                if let Some(ref calls) = choice.delta.tool_calls {
                    for call in calls {
                        if !tool_calls.iter().any(|c| c.id == call.id) {
                            tool_calls.push(call.clone());
                        }
                    }
                }
            }
        }

        if had_tools {
            tracing::debug!(
                "Non-streaming chat raw response: content={:?} thinking={:?} finish_reason={} native_tool_calls={}",
                full_content.chars().take(200).collect::<String>(),
                full_thinking.chars().take(200).collect::<String>(),
                finish_reason,
                tool_calls.len(),
            );
            // Native tool calls (parsed from SSE type:2 agent_message.tool_calls)
            // are the primary path. Only fall back to XML marker parsing when
            // no native calls were produced.
            if tool_calls.is_empty() {
                // Try MiniMax-native format first, then fall back to MTP
                // blocks (the universal dialect).
                let (cleaned, parsed) = parse_minimax_tool_calls(&full_content);
                if let Some(calls) = parsed {
                    if !calls.is_empty() {
                        tool_calls = calls;
                        full_content = cleaned;
                    }
                } else {
                    let defs: Vec<crate::models::Tool> =
                        request.tools.clone().unwrap_or_default();
                    let mut st = crate::providers::mtp::MtpStreamState::new();
                    let cleaned = st.process_delta(&full_content, &defs);
                    st.finish(&defs);
                    let calls = std::mem::take(&mut st.collected_tool_calls);
                    if !calls.is_empty() {
                        tool_calls = calls;
                        full_content = cleaned;
                    }
                }
            }
        }

        // Session is multi-turn; keep it cached for reuse
        let prompt_text: String = request
            .messages
            .iter()
            .map(|m| m.content.as_text())
            .collect();
        let usage = estimate_cost(&prompt_text, &full_content);

        let has_tool_calls = !tool_calls.is_empty();
        if has_tool_calls {
            finish_reason = "tool_calls".to_string();
            self.store.store_tool_calls(gateway_session_id, &tool_calls).await;
        } else if finish_reason == "tool_calls" {
            // SSE finish said "tool_calls"/"toolUse" but no tool calls were parsed
            finish_reason = "stop".to_string();
        }

        Ok(ChatCompletionResponse {
            id: response_id,
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model,
            choices: vec![crate::models::ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: ChatContent::String(full_content),
                    name: None,
                    reasoning_content: if full_thinking.is_empty() {
                        None
                    } else {
                        Some(full_thinking)
                    },
                    citations: None,
                    tool_calls: if has_tool_calls { Some(tool_calls) } else { None },
                    tool_call_id: None,
                },
                finish_reason,
            }],
            usage,
            session_url: None,
        })
    }

    /// Streaming chat. Forwards SSE deltas as ChatCompletionChunks in real time.
    pub async fn chat_stream(
        &self,
        request: ChatCompletionRequest,
        gateway_session_id: &str,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let response_id = format!("chatcmpl-{}", &new_uuid()[..8]);
        let model_id = request.model.clone();
        let attachments = self.process_attachments(&request).await?;
        let session_state = self.get_or_create_session(gateway_session_id).await?;

        let raw_stream = self
            .send_message_sse(&session_state, &request, &attachments, gateway_session_id)
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

        tokio::spawn({
            let tx = tx.clone();
            let response_id = response_id.clone();
            let model_id = model_id.clone();
            async move {
                let mut prev_content = String::new();
                let mut prev_thinking = String::new();
                let mut role_emitted = false;
                let mut finish_emitted = false;
                let mut tool_calls: Vec<ToolCall> = Vec::new();

                let mut stream = raw_stream;
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };

                    let mut delta_content: Option<String> = None;
                    let mut delta_thinking: Option<String> = None;
                    let mut delta_tool_calls: Option<Vec<ToolCall>> = None;
                    let mut fr: Option<String> = None;

                    for choice in chunk.choices {
                        if let Some(ref c) = choice.delta.content {
                            let new = if c.starts_with(&prev_content) {
                                c[prev_content.len()..].to_string()
                            } else {
                                c.clone()
                            };
                            if !new.is_empty() {
                                delta_content = Some(new);
                            }
                            prev_content = c.clone();
                        }
                        if let Some(ref t) = choice.delta.reasoning_content {
                            let new = if t.starts_with(&prev_thinking) {
                                t[prev_thinking.len()..].to_string()
                            } else {
                                t.clone()
                            };
                            if !new.is_empty() {
                                delta_thinking = Some(new);
                            }
                            prev_thinking = t.clone();
                        }
                        if let Some(ref tc) = choice.delta.tool_calls {
                            delta_tool_calls = Some(tc.clone());
                            for call in tc {
                                if !tool_calls.iter().any(|c| c.id == call.id) {
                                    tool_calls.push(call.clone());
                                }
                            }
                        }
                        if let Some(ref f) = choice.finish_reason {
                            fr = Some(f.clone());
                            finish_emitted = true;
                        }
                    }

                    if delta_content.is_none() && delta_thinking.is_none() && delta_tool_calls.is_none() && fr.is_none() {
                        continue;
                    }

                    let role = if !role_emitted {
                        role_emitted = true;
                        Some("assistant".to_string())
                    } else {
                        None
                    };

                    let _ = tx.send(Ok(ChatCompletionChunk {
                        id: response_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created: current_timestamp(),
                        model: model_id.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: ChatMessageDelta {
                                role,
                                content: delta_content,
                                reasoning_content: delta_thinking,
                                citations: None,
                                tool_calls: delta_tool_calls,
                            },
                            finish_reason: fr,
                        }],
                        session_url: None,
                    }));
                }

                if !finish_emitted {
                    let _ = tx.send(Ok(ChatCompletionChunk {
                        id: format!("{}-final", response_id),
                        object: "chat.completion.chunk".to_string(),
                        created: current_timestamp(),
                        model: model_id.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: ChatMessageDelta::default(),
                            finish_reason: Some("stop".to_string()),
                        }],
                        session_url: None,
                    }));
                }

            }
        });

        Ok(UnboundedReceiverStream::new(rx).boxed())
    }

    async fn handle_tool_results(&self, gateway_session_id: &str, messages: &[ChatMessage]) -> String {
        let tool_msgs: Vec<&ChatMessage> = messages.iter().filter(|m| m.role == "tool").collect();
        if tool_msgs.is_empty() {
            return String::new();
        }

        let mut items = Vec::new();
        for msg in tool_msgs {
            let call = match msg.tool_call_id.as_deref() {
                Some(id) => self.store.get_tool_call(gateway_session_id, id).await,
                None => None,
            };
            items.push((call, msg.tool_call_id.clone(), msg.content.as_text()));
        }

        let refs: Vec<(Option<&ToolCall>, Option<&str>, &str)> = items
            .iter()
            .map(|(c, id, o)| (c.as_ref(), id.as_deref(), o.as_str()))
            .collect();

        crate::providers::mtp::format_tool_results(&refs)
    }

    /// Process file attachments: upload them via Minimax's file API and
    /// return structured `FileAttachment` objects for the message payload.
    async fn process_attachments(&self, request: &ChatCompletionRequest) -> Result<Vec<super::upload::FileAttachment>, GatewayError> {
        let all_urls: Vec<String> = request
            .messages
            .iter()
            .flat_map(|m| m.content.image_urls())
            .chain(request.messages.iter().flat_map(|m| m.content.file_urls()))
            .collect();

        if all_urls.is_empty() {
            return Ok(Vec::new());
        }

        let mut processed = Vec::with_capacity(all_urls.len());
        for url in &all_urls {
            match super::upload::resolve_url(url, &self.upload_cache).await {
                Ok(resolved) => processed.push(resolved),
                Err(e) => {
                    tracing::warn!(url = %url, error = %e, "Minimax upload failed, skipping attachment");
                }
            }
        }
        Ok(processed)
    }

    async fn send_message_sse(
        &self,
        session_state: &MinimaxSessionState,
        request: &ChatCompletionRequest,
        attachments: &[super::upload::FileAttachment],
        gateway_session_id: &str,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError>
    {
        // Wait for session to be idle before sending a message
        self.poll_session_idle(&session_state.session_id).await?;

        // Build content with structured conversation context (matching Python reference)
        let mut content = String::new();
        let mut ctx_parts: Vec<String> = Vec::new();
        for m in request.messages.iter().take(request.messages.len().saturating_sub(1)) {
            let text = m.content.as_text();
            if text.is_empty() {
                continue;
            }
            let label = match m.role.as_str() {
                "system" => "系统指令",
                "assistant" => "助手",
                "user" => "用户",
                "tool" => "工具",
                _ => m.role.as_str(),
            };
            ctx_parts.push(format!("{}: {}", label, text));
        }
        if let Some(last) = request.messages.last() {
            let last_text = last.content.as_text();
            if ctx_parts.is_empty() {
                content = last_text;
            } else {
                content = format!("{}\n\n用户说: {}", ctx_parts.join("\n\n"), last_text);
            }
        }

        let tool_context = self.handle_tool_results(gateway_session_id, &request.messages).await;
        if !tool_context.is_empty() {
            content = format!("{}\n\n{}", tool_context, content);
        }

        // Compile client tools into the MTP system prompt. Native
        // `tools` are never forwarded upstream (the endpoint ignores them;
        // the model emits [MIRAGE_TOOL_CALL_V1] blocks instead).
        if let Some(tools) = &request.tools {
            if !tools.is_empty() {
                content = format!(
                    "{}\n\nUser request:\n{}",
                    crate::providers::mtp::build_mtp_system_prompt(
                        tools,
                        request.tool_choice.as_ref(),
                        false
                    ),
                    content
                );
            }
        }

        // Build structured attachments array matching web app format
        let attach_values: Vec<serde_json::Value> = attachments
            .iter()
            .map(|a| {
                serde_json::json!({
                    "type": a.file_type,
                    "file_path": a.file_path,
                    "file_name": a.file_name,
                    "mime_type": a.mime_type,
                    "data_url": a.data_url,
                })
            })
            .collect();

        let had_tools = request.tools.is_some();
        let model_name = upstream_model(&self.model_id);
        let variant = variant_for_model(&self.model_id, request.thinking);

        let mut payload = serde_json::json!({
            "content": content,
            "model": {
                "provider_id": "minimax",
                "model_id": model_name,
                "variant": variant,
            },
            "turn_id": new_uuid(),
            "worktreeMode": "false",
        });

        // Attachments array — web app sends structured `attachments` field
        if !attach_values.is_empty() {
            payload["attachments"] = serde_json::Value::Array(attach_values);
        }

        // OpenAI tools/tool_choice are never forwarded upstream (MTP
        // invariant): the compiled prompt carries the tool contract.
        let body_str = serde_json::to_string(&payload)
            .map_err(|e| GatewayError::Internal(format!("JSON serialization failed: {e}")))?;

        let path = format!(
            "{}/session/{}/message",
            AGENT_BASE,
            session_state.session_id
        );
        let ts_ms = unix_ms();
        let timezone_offset_secs = Local::now().offset().local_minus_utc() as i64;

        // Single query string used for BOTH yy computation and the actual URL
        let query = build_query_string(
            &self.jwt,
            &session_state.uuid,
            &session_state.device_id,
            &session_state.user_id,
            ts_ms,
            timezone_offset_secs,
        );
        let uri = build_uri(&path, &query);
        let url = format!("{}{}", STREAM_HOST, uri);
        let auth_headers = build_auth_headers(&self.jwt, &body_str, &uri, ts_ms);

        let payload_preview = body_str.chars().take(500).collect::<String>();
        tracing::info!(
            url = %url,
            body = %payload_preview,
            "Sending Minimax message SSE request"
        );

        let client = build_reqwest_client()?;
        let mut req_builder = client.post(&url);
        for (k, v) in &auth_headers {
            req_builder = req_builder.header(k.as_str(), v.as_str());
        }
        // Include cookies from the session so the ak_bmsc cookie set during
        // session creation is sent with the SSE message request.
        let parsed_url = url::Url::parse(&url)
            .map_err(|e| GatewayError::Internal(format!("invalid SSE URL: {e}")))?;
        let cookie_header = self.session.cookie_jar.get_cookie_header(&parsed_url);
        if !cookie_header.is_empty() {
            tracing::debug!(cookie_len = cookie_header.len(), "Forwarding session cookies to SSE request");
            req_builder = req_builder.header("cookie", &cookie_header);
        }
        req_builder = req_builder.body(body_str.clone());

        let response = req_builder.send().await.map_err(|e| {
            GatewayError::Provider(format!("Minimax SSE request failed: {e}"))
        })?;

        tracing::info!(status = %response.status(), "Minimax SSE response received");

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_else(|_| format!("HTTP {status}"));
            return Err(GatewayError::Provider(format!(
                "Minimax SSE request returned {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }

        let (tx, rx) = mpsc::unbounded_channel::<Result<ChatCompletionChunk, GatewayError>>();

        // Clone tool defs for the spawned task (request borrow cannot cross).
        let stream_tool_defs = request.tools.clone().unwrap_or_default();
        tokio::spawn({
            let tx = tx.clone();
            async move {
                let mut buf: Vec<u8> = Vec::with_capacity(4096);
                let mut http_chunks = response.bytes_stream();
                let mut state = StreamState {
                    mtp_state: crate::providers::mtp::MtpStreamState::new(),
                    tool_defs: stream_tool_defs,
                    collected_tool_calls: Vec::new(),
                };
                let mut saw_tool_calls = false;

                tracing::debug!("SSE stream reading started");
                while let Some(chunk_result) = http_chunks.next().await {
                    let chunk = match chunk_result {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(GatewayError::Provider(format!(
                                "Minimax stream read error: {e}"
                            ))));
                            return;
                        }
                    };
                    buf.extend_from_slice(&chunk);

                    // Drain all complete SSE events from the buffer.
                    loop {
                        let boundary = match find_sse_boundary(&buf) {
                            Some(b) => b,
                            None => break,
                        };

                        let event = buf[..boundary].to_vec();
                        buf.drain(..boundary);

                        let text = match String::from_utf8(event) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };

                        let json_str = match parse_sse_line(&text) {
                            Some(s) if !s.is_empty() && s != "[DONE]" => s,
                            _ => continue,
                        };

                        let event: SseEvent = match serde_json::from_str(json_str) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(error = %e, raw = %json_str.chars().take(100).collect::<String>(), "SSE parse error");
                                continue;
                            }
                        };

                        match event.event_type {
                                6 => {
                                // Streaming chunk (thinking + content + finish)
                                let chunk = match &event.agent_message_chunk {
                                    Some(c) => c,
                                    None => continue,
                                };
                                let content_text = chunk.msg_content.as_deref().unwrap_or("");
                                let thinking_text = chunk.thinking_content.as_deref().unwrap_or("");

                                // Log raw event to capture error details we
                                // might not be deserializing.
                                if chunk.finish.unwrap_or(false) {
                                    tracing::warn!(
                                        "SSE type:6 raw: {}",
                                        json_str.chars().take(500).collect::<String>()
                                    );
                                }
                                tracing::debug!(
                                    "SSE type:6 chunk: content_len={} thinking_len={} finish={:?} finish_reason={:?}",
                                    content_text.len(),
                                    thinking_text.len(),
                                    chunk.finish,
                                    chunk.finish_reason,
                                );

                                // Process content for tool call markers (both <tool_call> and <minimax:tool_call>).
                                let cleaned = if had_tools && !content_text.is_empty() {
                                    let processed = process_content_text(content_text, &mut state);
                                    if content_text.contains("<minimax:tool_call>") {
                                        let (stripped, calls) = parse_minimax_tool_calls(&processed);
                                        if let Some(c) = calls {
                                            for tc in c {
                                                state.collected_tool_calls.push(tc);
                                            }
                                        }
                                        stripped
                                    } else {
                                        processed
                                    }
                                } else {
                                    content_text.to_string()
                                };

                                while let Some(tc) = state.collected_tool_calls.pop() {
                                    saw_tool_calls = true;
                                    let _ = tx.send(Ok(ChatCompletionChunk {
                                        id: uuid::Uuid::new_v4().to_string(),
                                        object: "chat.completion.chunk".to_string(),
                                        created: current_timestamp(),
                                        model: String::new(),
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
                                        session_url: None,
                                    }));
                                }

                                let is_finish = chunk.finish.unwrap_or(false);
                                let fr = if is_finish {
                                    let raw = chunk.finish_reason.clone().unwrap_or_else(|| "stop".to_string());

                                    if raw == "error" {
                                        tracing::error!(
                                            finish_reason = "error",
                                            raw = %json_str.chars().take(1000).collect::<String>(),
                                            "Minimax stream returned finish_reason=error"
                                        );
                                    }

                                    tracing::debug!("SSE finish: raw={raw} saw_tool_calls={saw_tool_calls} content_len={}", content_text.len());
                                    if saw_tool_calls { "tool_calls".to_string() }
                                    else if raw == "toolUse" { "tool_calls".to_string() }
                                    else { raw }
                                } else {
                                    String::new()
                                };

                                // Don't immediately error on type:6 finish_reason: error —
                                // the actual error message may arrive in a subsequent type:2 event.
                                let is_error = fr == "error";
                                if is_finish && is_error {
                                    tracing::debug!("type:6 signaled error, awaiting type:2 for details");
                                }

                                if !cleaned.is_empty() || !thinking_text.is_empty() || is_finish {
                                    let _ = tx.send(Ok(ChatCompletionChunk {
                                        id: uuid::Uuid::new_v4().to_string(),
                                        object: "chat.completion.chunk".to_string(),
                                        created: current_timestamp(),
                                        model: String::new(),
                                        choices: vec![ChunkChoice {
                                            index: 0,
                                            delta: ChatMessageDelta {
                                                role: Some("assistant".to_string()),
                                                content: if cleaned.is_empty() { None } else { Some(cleaned) },
                                                reasoning_content: if thinking_text.is_empty() { None } else { Some(thinking_text.to_string()) },
                                                citations: None,
                                                tool_calls: None,
                                            },
                                            finish_reason: if is_finish { Some(fr) } else { None },
                                        }],
                                        session_url: None,
                                    }));
                                }

                                if is_finish && !is_error {
                                    return;
                                }
                            }
                             2 => {
                                // Agent message (echo or final). May carry metadata in tool_calls
                                // or a finish_reason with error details.
                                if let Some(ref am) = event.agent_message {
                                    let is_error = am.finish_reason.as_deref() == Some("error");
                                    if is_error {
                                        let err_msg = am.msg_content.as_deref().unwrap_or("unknown");
                                        let full_msg = format!(
                                            "Minimax returned finish_reason: error — {err_msg}"
                                        );
                                        let full_msg = if super::is_minimax_quota_exhaustion(&full_msg) {
                                            super::augment_quota_message(&full_msg)
                                        } else {
                                            full_msg
                                        };
                                        let _ = tx.send(Err(GatewayError::Provider(full_msg)));
                                        return;
                                    }
                                    tracing::debug!(
                                        "SSE type:2 agent_message: msg_type={:?} role={:?} msg_content_len={} has_tool_calls={} raw={}",
                                        am.msg_type,
                                        am.role,
                                        am.msg_content.as_ref().map(|c| c.len()).unwrap_or(0),
                                        am.tool_calls.is_some(),
                                        serde_json::to_string(am).unwrap_or_default().chars().take(500).collect::<String>(),
                                    );
                                    if let Some(ref tc) = am.tool_calls {
                                        tracing::debug!("SSE type:2 native tool_calls: {tc}");
                                        // Parse native tool_calls from agent_message
                                        if let Ok(calls) = parse_native_tool_calls(tc) {
                                            for call in calls {
                                                saw_tool_calls = true;
                                                let _ = tx.send(Ok(ChatCompletionChunk {
                                                    id: uuid::Uuid::new_v4().to_string(),
                                                    object: "chat.completion.chunk".to_string(),
                                                    created: current_timestamp(),
                                                    model: String::new(),
                                                    choices: vec![ChunkChoice {
                                                        index: 0,
                                                        delta: ChatMessageDelta {
                                                            role: None,
                                                            content: None,
                                                            reasoning_content: None,
                                                            citations: None,
                                                            tool_calls: Some(vec![call]),
                                                        },
                                                        finish_reason: None,
                                                    }],
                                                    session_url: None,
                                                }));
                                            }
                                        }
                                    }
                                }
                            }
                            10 => {
                                // Session init notification; skip.
                            }
                            _ => {
                                tracing::debug!("SSE unknown event type: {} raw={}", event.event_type, json_str.chars().take(500).collect::<String>());
                            }
                        }
                    }
                }
                tracing::debug!("SSE stream loop ended");
            }
        });

        Ok(UnboundedReceiverStream::new(rx).boxed())
    }
}

/// Find a Minimax JWT in the session's localStorage, with env var fallback.
fn find_jwt(session: &SessionHandle) -> Option<String> {
    const KNOWN_KEYS: &[&str] = &[
        "_token",
        "mavis:token",
    ];

    for entry in &session.local_storage {
        let key_lower = entry.key.to_lowercase();
        if KNOWN_KEYS.iter().any(|k| key_lower == *k) {
            let val = entry.value.trim();
            if val.split('.').count() == 3 && val.len() > 30 {
                tracing::info!("Found Minimax JWT in localStorage key '{}'", entry.key);
                return Some(val.to_string());
            }
        }
    }

    // Fallback: scan all entries for JWT-like values.
    for entry in &session.local_storage {
        let val = entry.value.trim();
        let parts: Vec<&str> = val.splitn(3, '.').collect();
        if parts.len() == 3
            && parts[0].len() > 10
            && parts[1].len() > 10
            && parts[2].len() > 10
        {
            tracing::info!("Found Minimax JWT via fallback scan in localStorage key '{}'", entry.key);
            return Some(val.to_string());
        }
    }

    // Fallback: MINIMAX_JWT env var.
    if let Ok(jwt) = std::env::var("MINIMAX_JWT") {
        let jwt = jwt.trim().to_string();
        if jwt.split('.').count() == 3 && jwt.len() > 30 {
            tracing::info!("Found Minimax JWT from MINIMAX_JWT env var");
            return Some(jwt);
        }
    }

    tracing::warn!("Minimax JWT not found in localStorage or MINIMAX_JWT env var");
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::LocalStorageEntry;
    use obscura_net::CookieJar;
    use std::sync::Arc;

    fn make_session(local_storage: Vec<LocalStorageEntry>) -> SessionHandle {
        SessionHandle {
            id: "test".into(),
            cookie_jar: Arc::new(CookieJar::new()),
            local_storage,
            user_agent: "test".into(),
        }
    }

    #[test]
    fn variant_for_model_honors_thinking_toggle() {
        // M3 is switchable: explicit off maps to the captured web-app default
        // ("") and on maps to "thinking".
        assert_eq!(variant_for_model("minimax-m3", Some(false)), "");
        assert_eq!(variant_for_model("minimax-m3", Some(true)), "thinking");
        assert_eq!(variant_for_model("minimax-m3", None), "thinking");
        // M2.7 reasoning is forced on; validate_request rejects thinking:false
        // before the payload is built, so the variant stays "thinking".
        assert_eq!(variant_for_model("minimax-m2.7", Some(true)), "thinking");
        assert_eq!(variant_for_model("minimax-m2.7", None), "thinking");
        assert_eq!(variant_for_model("minimax-m2.7-highspeed", Some(true)), "thinking");
    }

    const VALID_JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJleHAiOjk5OTk5OTk5OTksInVzZXIiOnsiaWQiOiJ0ZXN0IiwibmFtZSI6InRlc3QiLCJkZXZpY2VJRCI6InRlc3QiLCJpc0Fub255bW91cyI6ZmFsc2V9fQ.p0W0bc_S_XPPnwQdYJAPOF1-3CoTluBVPxHzgNFVr_0";

    #[test]
    fn find_jwt_from_local_storage() {
        let entry = LocalStorageEntry {
            origin: "https://agent.minimax.io".into(),
            key: "_token".into(),
            value: VALID_JWT.into(),
        };
        let session = make_session(vec![entry]);
        assert_eq!(find_jwt(&session).as_deref(), Some(VALID_JWT));
    }

    #[test]
    fn find_jwt_ignores_non_jwt_values() {
        let entry = LocalStorageEntry {
            origin: "https://agent.minimax.io".into(),
            key: "_token".into(),
            value: "not-a-jwt".into(),
        };
        let session = make_session(vec![entry]);
        assert!(find_jwt(&session).is_none());
    }

    #[test]
    fn find_jwt_from_env_var() {
        let old = std::env::var("MINIMAX_JWT").ok();
        std::env::set_var("MINIMAX_JWT", VALID_JWT);
        let session = make_session(vec![]);
        let result = find_jwt(&session);
        // Restore before asserting to avoid polluting if the assert fails.
        match old {
            Some(v) => std::env::set_var("MINIMAX_JWT", v),
            None => std::env::remove_var("MINIMAX_JWT"),
        }
        assert_eq!(result.as_deref(), Some(VALID_JWT));
    }

    #[test]
    fn find_jwt_env_var_ignores_invalid_jwt() {
        let old = std::env::var("MINIMAX_JWT").ok();
        std::env::set_var("MINIMAX_JWT", "not-a-jwt");
        let session = make_session(vec![]);
        let result = find_jwt(&session);
        match old {
            Some(v) => std::env::set_var("MINIMAX_JWT", v),
            None => std::env::remove_var("MINIMAX_JWT"),
        }
        assert!(result.is_none());
    }

    #[test]
    fn find_jwt_prefers_local_storage_over_env_var() {
        let old = std::env::var("MINIMAX_JWT").ok();
        // Set env var to one JWT...
        std::env::set_var("MINIMAX_JWT", VALID_JWT);
        // But localStorage has a different (also valid) JWT.
        let ls_jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJleHAiOjk5OTk5OTk5OTksInVzZXIiOnsiaWQiOiJsb2NhbC1zdG9yYWdlIiwibmFtZSI6InRlc3QiLCJkZXZpY2VJRCI6InRlc3QiLCJpc0Fub255bW91cyI6ZmFsc2V9fQ.ls-storage-signature";
        let entry = LocalStorageEntry {
            origin: "https://agent.minimax.io".into(),
            key: "_token".into(),
            value: ls_jwt.into(),
        };
        let session = make_session(vec![entry]);
        let result = find_jwt(&session);
        match old {
            Some(v) => std::env::set_var("MINIMAX_JWT", v),
            None => std::env::remove_var("MINIMAX_JWT"),
        }
        assert_eq!(result.as_deref(), Some(ls_jwt));
    }
}

fn estimate_cost(prompt: &str, completion: &str) -> Usage {
    let prompt_tokens = (prompt.len() / 4).max(1) as i32;
    let completion_tokens = (completion.len() / 4).max(1) as i32;
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    }
}
