//! Provider Protocol Registry.
//!
//! Each provider declares a versioned manifest describing its wire protocol:
//! auth requirements, endpoints, streaming format, feature capabilities,
//! error classification rules, and rate-limit policy. The manifest is loaded
//! once at startup and queried at runtime, replacing hardcoded per-provider
//! special cases.
//!
//! Manifests are built from captured evidence (see `docs/wire/`). A manifest
//! that says `Unknown` for a capability means "not yet verified by capture",
//! not "does not exist". The adapter code may still attempt the feature via
//! its existing runtime path; the manifest simply does not override it.

use serde::{Deserialize, Serialize};

use super::health::RateConfig;

/// Capability verdict from live evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Confirmed working by capture.
    Yes,
    /// Confirmed absent by capture (provider does not expose this).
    No,
    /// Not yet verified; adapter uses its existing runtime path.
    Unknown,
}

/// How session continuation works for this provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationMode {
    /// Provider uses a conversation ID that the gateway stores.
    NativeConversationId,
    /// Provider uses a message-tree structure.
    NativeMessageTree,
    /// Gateway must resend full history (provider has no continuation IDs).
    GatewayHistoryRequired,
    /// Continuation not supported or not yet verified.
    Unsupported,
}

/// Streaming wire format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// Standard `data: {...}\n\n` SSE.
    #[default]
    Sse,
    /// JSON lines (one JSON object per line, no `data:` prefix).
    JsonLines,
    /// Length-prefixed binary frames (e.g. Gemini protobuf).
    Framed,
    /// WebSocket text frames containing JSON.
    WebSocket,
}

/// A single cookie requirement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieRequirement {
    /// Domain to match (without leading dot).
    pub domain: String,
    /// Cookie name.
    pub name: String,
    /// If true, at least one of a set of alternatives must be present.
    #[serde(default)]
    pub any_of: bool,
}

/// A single localStorage requirement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalStorageRequirement {
    /// Origin URL.
    pub origin: String,
    /// Key name.
    pub key: String,
    /// If true, at least one of a set of alternatives must be present.
    #[serde(default)]
    pub any_of: bool,
}

/// Auth specification for a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthSpec {
    /// Cookies that must be present. All listed cookies are required
    /// unless marked `any_of`.
    #[serde(default)]
    pub cookies: Vec<CookieRequirement>,
    /// localStorage entries required. All listed entries are required
    /// unless marked `any_of`.
    #[serde(default)]
    pub local_storage: Vec<LocalStorageRequirement>,
    /// Environment variable that satisfies auth (e.g. MINIMAX_JWT).
    #[serde(default)]
    pub env_var: Option<String>,
}

/// Endpoint templates for a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointSpec {
    /// Base URL for the provider web app.
    pub base_url: String,
    /// Chat/completion endpoint path.
    pub chat: String,
    /// File upload endpoint path (if supported).
    #[serde(default)]
    pub files: Option<String>,
    /// New chat/conversation creation endpoint path (if supported).
    #[serde(default)]
    pub new_chat: Option<String>,
}

impl Default for EndpointSpec {
    fn default() -> Self {
        EndpointSpec {
            base_url: String::new(),
            chat: String::new(),
            files: None,
            new_chat: None,
        }
    }
}

/// Streaming configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamSpec {
    /// Wire format kind.
    #[serde(default)]
    pub kind: StreamKind,
    /// Line prefix before JSON payload (e.g. `"data: "` for SSE).
    #[serde(default)]
    pub line_prefix: Option<String>,
    /// JSONPath expression to the content delta in a stream event.
    #[serde(default)]
    pub delta_path: Option<String>,
    /// JSONPath expression to the reasoning/thinking content.
    #[serde(default)]
    pub reasoning_path: Option<String>,
    /// JSONPath to finish reason.
    #[serde(default)]
    pub finish_reason_path: Option<String>,
    /// Signal that the stream is complete (event type or marker).
    #[serde(default)]
    pub done_signal: Option<String>,
}

/// Feature capabilities for a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSpec {
    /// Native function/tool calling via the `tools` API parameter.
    pub native_tools: Capability,
    /// Native file upload via provider API.
    pub native_upload: Capability,
    /// Native thinking/reasoning output.
    pub native_thinking: Capability,
    /// Native web search with citation extraction.
    pub native_search: Capability,
    /// Session continuation mode.
    pub continuation: ContinuationMode,
}

/// Rate-limit configuration overrides for this provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateSpec {
    /// Maximum concurrent in-flight requests.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    /// Sustained token refill rate (requests per minute).
    #[serde(default = "default_requests_per_minute")]
    pub requests_per_minute: u32,
    /// Burst capacity above sustained rate.
    #[serde(default = "default_burst")]
    pub burst: u32,
    /// Rolling hourly request cap (0 = disabled).
    #[serde(default)]
    pub messages_per_hour: u32,
    /// Cooldown after a 429 (seconds).
    #[serde(default = "default_cooldown_429")]
    pub cooldown_after_429_secs: u64,
}

fn default_max_concurrency() -> usize { 4 }
fn default_requests_per_minute() -> u32 { 60 }
fn default_burst() -> u32 { 4 }
fn default_cooldown_429() -> u64 { 60 }

impl Default for RateSpec {
    fn default() -> Self {
        RateSpec {
            max_concurrency: default_max_concurrency(),
            requests_per_minute: default_requests_per_minute(),
            burst: default_burst(),
            messages_per_hour: 0,
            cooldown_after_429_secs: default_cooldown_429(),
        }
    }
}

impl RateSpec {
    /// Convert to the runtime `RateConfig` used by the rate limiter.
    pub fn to_rate_config(&self) -> RateConfig {
        RateConfig {
            max_concurrency: self.max_concurrency,
            requests_per_minute: self.requests_per_minute,
            burst: self.burst,
            messages_per_hour: self.messages_per_hour,
            cooldown_after_429: std::time::Duration::from_secs(self.cooldown_after_429_secs),
        }
    }
}

/// Complete protocol manifest for one provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderManifest {
    /// Provider name (e.g. "deepseek").
    pub name: String,
    /// Manifest schema version (for migration).
    pub version: u32,
    /// Auth requirements.
    #[serde(default)]
    pub auth: AuthSpec,
    /// Endpoint templates.
    #[serde(default)]
    pub endpoints: EndpointSpec,
    /// Streaming configuration.
    #[serde(default)]
    pub stream: StreamSpec,
    /// Feature capabilities.
    pub features: FeatureSpec,
    /// Rate-limit overrides.
    #[serde(default)]
    pub rate: RateSpec,
}

impl ProviderManifest {
    /// Convert the manifest's `RateSpec` into a runtime `RateConfig`.
    pub fn rate_config(&self) -> RateConfig {
        self.rate.to_rate_config()
    }
}

/// Built-in manifests for all 12 providers.
///
/// These are compiled from captured evidence in `docs/wire/`. Providers
/// without a full capture have `Capability::Unknown` for unverified
/// features; the adapter code handles the runtime path regardless.
pub fn builtin_manifests() -> Vec<ProviderManifest> {
    vec![
        ProviderManifest {
            name: "deepseek".into(),
            version: 1,
            auth: AuthSpec {
                local_storage: vec![LocalStorageRequirement {
                    origin: "https://chat.deepseek.com".into(),
                    key: "userToken".into(),
                    any_of: false,
                }],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://chat.deepseek.com".into(),
                chat: "/api/chat/completions".into(),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("data: ".into()),
                delta_path: Some("choices[0].delta.content".into()),
                reasoning_path: Some("choices[0].delta.reasoning_content".into()),
                finish_reason_path: Some("choices[0].finish_reason".into()),
                done_signal: Some("[DONE]".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::No,
                native_upload: Capability::Yes,
                native_thinking: Capability::Yes,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "chatgpt".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "chatgpt.com".into(), name: "__Secure-next-auth.session-token".into(), any_of: true },
                    CookieRequirement { domain: "chatgpt.com".into(), name: "__Host-next-auth.csrf-token".into(), any_of: true },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://chatgpt.com".into(),
                chat: "/backend-api/conversation".into(),
                files: Some("/backend-api/files".into()),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("data: ".into()),
                delta_path: Some("v".into()),
                finish_reason_path: Some("v[0].finish_details.type".into()),
                done_signal: Some("done".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::Unknown,
                native_upload: Capability::Yes,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "claude".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "claude.ai".into(), name: "sessionKey".into(), any_of: false },
                    CookieRequirement { domain: "claude.ai".into(), name: "lastActiveOrg".into(), any_of: false },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://claude.ai".into(),
                chat: "/api/organizations/{org_id}/chat_conversations/{chat_id}/completion".into(),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("event: ".into()),
                delta_path: Some("delta".into()),
                done_signal: Some("message_stop".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::Unknown,
                native_upload: Capability::Unknown,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "gemini".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "gemini.google.com".into(), name: "__Secure-1PSID".into(), any_of: true },
                    CookieRequirement { domain: ".google.com".into(), name: "SID".into(), any_of: true },
                    CookieRequirement { domain: ".google.com".into(), name: "HSID".into(), any_of: true },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://gemini.google.com".into(),
                chat: "/_/BardFrontendService/StreamGenerate".into(),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Framed,
                delta_path: Some("[].simd_json_path".into()),
                done_signal: Some("".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::No,
                native_upload: Capability::Yes,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "glm".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "chat.z.ai".into(), name: "token".into(), any_of: true },
                ],
                local_storage: vec![
                    LocalStorageRequirement { origin: "https://chat.z.ai".into(), key: "token".into(), any_of: true },
                    LocalStorageRequirement { origin: "https://chat.z.ai".into(), key: "access_token".into(), any_of: true },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://chat.z.ai".into(),
                chat: "/api/v2/chat/completions".into(),
                files: Some("/api/v1/files/".into()),
                new_chat: Some("/api/v1/chats/new".into()),
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("data: ".into()),
                delta_path: Some("data.delta_content".into()),
                reasoning_path: Some("data.delta_content".into()),
                done_signal: Some("data.done".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::Unknown,
                native_upload: Capability::Yes,
                native_thinking: Capability::Yes,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "grok".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "x.ai".into(), name: "sso".into(), any_of: false },
                    CookieRequirement { domain: "x.ai".into(), name: "sso-rw".into(), any_of: false },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://grok.com".into(),
                chat: "/rest/app-chat/conversations/{conversation_id}/add-message".into(),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("data: ".into()),
                delta_path: Some("choices[0].delta.content".into()),
                finish_reason_path: Some("choices[0].finish_reason".into()),
                done_signal: Some("[DONE]".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::Unknown,
                native_upload: Capability::Unknown,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "kimi".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "kimi.com".into(), name: "kimi-auth".into(), any_of: true },
                    CookieRequirement { domain: "www.kimi.com".into(), name: "kimi-auth".into(), any_of: true },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://www.kimi.com".into(),
                chat: "/api/chat".into(),
                files: Some("/api/files".into()),
                ..Default::default()
            },
            stream: StreamSpec {
                // Legacy path is SSE; the kimi.ai web app now speaks
                // ConnectRPC (see providers/kimi/connectrpc.rs and
                // captures/kimi_chat_wire.txt). Framed reflects the
                // target transport; the legacy SSE path remains as
                // fallback until live verification flips the default.
                kind: StreamKind::Framed,
                line_prefix: Some("data: ".into()),
                delta_path: Some("choices[0].delta.content".into()),
                finish_reason_path: Some("choices[0].finish_reason".into()),
                done_signal: Some("[DONE]".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::Unknown,
                native_upload: Capability::Unknown,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "metaai".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "meta.ai".into(), name: "ecto_1_sess".into(), any_of: true },
                    CookieRequirement { domain: "meta.ai".into(), name: "datr".into(), any_of: true },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://www.meta.ai".into(),
                chat: "/api/dgw".into(),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::WebSocket,
                delta_path: Some("text".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::No,
                native_upload: Capability::Unknown,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "minimax".into(),
            version: 1,
            auth: AuthSpec {
                env_var: Some("MINIMAX_JWT".into()),
                local_storage: vec![
                    LocalStorageRequirement { origin: "https://agent.minimax.io".into(), key: "_token".into(), any_of: true },
                    LocalStorageRequirement { origin: "https://agent.minimax.io".into(), key: "mavis:token".into(), any_of: true },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://agent.minimax.io".into(),
                chat: "/api/chat/completions".into(),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("data: ".into()),
                delta_path: Some("choices[0].delta.content".into()),
                finish_reason_path: Some("choices[0].finish_reason".into()),
                done_signal: Some("[DONE]".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::Unknown,
                native_upload: Capability::Unknown,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "mistral".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "mistral.ai".into(), name: "ory_kratos_continuity".into(), any_of: true },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://chat.mistral.ai".into(),
                chat: "/api/chat".into(),
                files: Some("/api/files".into()),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("data: ".into()),
                delta_path: Some("choices[0].delta.content".into()),
                finish_reason_path: Some("choices[0].finish_reason".into()),
                done_signal: Some("[DONE]".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::No,
                native_upload: Capability::Unknown,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec {
                max_concurrency: 2,
                requests_per_minute: 10,
                burst: 2,
                messages_per_hour: 60,
                cooldown_after_429_secs: 120,
            },
        },
        ProviderManifest {
            name: "mimo".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "xiaomimimo.com".into(), name: "xiaomichatbot_serviceToken".into(), any_of: false },
                    CookieRequirement { domain: "xiaomimimo.com".into(), name: "userId".into(), any_of: false },
                    CookieRequirement { domain: "xiaomimimo.com".into(), name: "xiaomichatbot_ph".into(), any_of: false },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://aistudio.xiaomimimo.com".into(),
                chat: "/api/chat".into(),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("data: ".into()),
                delta_path: Some("choices[0].delta.content".into()),
                finish_reason_path: Some("choices[0].finish_reason".into()),
                done_signal: Some("[DONE]".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::No,
                native_upload: Capability::Unknown,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
        ProviderManifest {
            name: "qwen".into(),
            version: 1,
            auth: AuthSpec {
                cookies: vec![
                    CookieRequirement { domain: "chat.qwen.ai".into(), name: "token".into(), any_of: true },
                    CookieRequirement { domain: ".qwen.ai".into(), name: "cna".into(), any_of: true },
                ],
                ..Default::default()
            },
            endpoints: EndpointSpec {
                base_url: "https://chat.qwen.ai".into(),
                chat: "/api/chat/completions".into(),
                files: Some("/api/files".into()),
                ..Default::default()
            },
            stream: StreamSpec {
                kind: StreamKind::Sse,
                line_prefix: Some("data: ".into()),
                delta_path: Some("choices[0].delta.content".into()),
                finish_reason_path: Some("choices[0].finish_reason".into()),
                done_signal: Some("[DONE]".into()),
                ..Default::default()
            },
            features: FeatureSpec {
                native_tools: Capability::No,
                native_upload: Capability::Yes,
                native_thinking: Capability::Unknown,
                native_search: Capability::Unknown,
                continuation: ContinuationMode::NativeConversationId,
            },
            rate: RateSpec::default(),
        },
    ]
}

/// Look up a manifest by provider name.
pub fn find_manifest(name: &str) -> Option<ProviderManifest> {
    builtin_manifests().into_iter().find(|m| m.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_manifests_covers_all_providers() {
        let manifests = builtin_manifests();
        let names: Vec<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"deepseek"));
        assert!(names.contains(&"chatgpt"));
        assert!(names.contains(&"claude"));
        assert!(names.contains(&"gemini"));
        assert!(names.contains(&"glm"));
        assert!(names.contains(&"grok"));
        assert!(names.contains(&"kimi"));
        assert!(names.contains(&"metaai"));
        assert!(names.contains(&"minimax"));
        assert!(names.contains(&"mistral"));
        assert!(names.contains(&"mimo"));
        assert!(names.contains(&"qwen"));
        assert_eq!(manifests.len(), 12);
    }

    #[test]
    fn manifest_serialization_roundtrip() {
        for manifest in builtin_manifests() {
            let json = serde_json::to_string(&manifest).unwrap();
            let parsed: ProviderManifest = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.name, manifest.name);
            assert_eq!(parsed.version, manifest.version);
            assert_eq!(parsed.features.native_tools, manifest.features.native_tools);
        }
    }

    #[test]
    fn find_manifest_returns_known_providers() {
        assert!(find_manifest("deepseek").is_some());
        assert!(find_manifest("chatgpt").is_some());
        assert!(find_manifest("nonexistent").is_none());
    }

    #[test]
    fn rate_config_conversion() {
        let manifest = find_manifest("mistral").unwrap();
        let cfg = manifest.rate_config();
        assert_eq!(cfg.max_concurrency, 2);
        assert_eq!(cfg.requests_per_minute, 10);
        assert_eq!(cfg.cooldown_after_429, std::time::Duration::from_secs(120));
    }

    #[test]
    fn default_rate_config_is_sensible() {
        let manifest = find_manifest("deepseek").unwrap();
        let cfg = manifest.rate_config();
        assert_eq!(cfg.max_concurrency, 4);
        assert_eq!(cfg.requests_per_minute, 60);
        assert_eq!(cfg.burst, 4);
    }

    #[test]
    fn auth_spec_has_required_fields() {
        let manifest = find_manifest("claude").unwrap();
        assert_eq!(manifest.auth.cookies.len(), 2);
        assert!(!manifest.auth.cookies[0].any_of);
        assert!(!manifest.auth.cookies[1].any_of);
    }

    #[test]
    fn auth_spec_any_of_cookies() {
        let manifest = find_manifest("chatgpt").unwrap();
        assert!(manifest.auth.cookies.iter().all(|c| c.any_of));
    }

    #[test]
    fn endpoint_spec_has_chat_path() {
        for manifest in builtin_manifests() {
            assert!(!manifest.endpoints.chat.is_empty(), "{} has empty chat path", manifest.name);
            assert!(!manifest.endpoints.base_url.is_empty(), "{} has empty base_url", manifest.name);
        }
    }
}
