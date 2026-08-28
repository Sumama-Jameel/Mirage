//! Per-provider/model capability profiles.
//!
//! Browser/internal provider APIs do not expose a native channel that
//! accepts arbitrary OpenAI `tools`; they ignore the `tools` parameter.
//! Therefore every provider uses the MTP/1 prompted tool-output dialect
//! (`tool_dialect: Mtp`), and OpenAI `tools` are never forwarded upstream
//! (`strip_upstream_tools: true`).
//!
//! The profile captures per-model quirks that affect how the MTP prompt is
//! compiled and how tool blocks are parsed, so provider adapters do not need
//! hardcoded special cases.

use serde::{Deserialize, Serialize};

/// Transport used to reach the provider's internal API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Standard `data: {...}\n\n` SSE.
    Sse,
    /// JSON lines (one JSON object per line, no `data:` prefix).
    JsonLines,
    /// Length-prefixed binary frames (e.g. ConnectRPC, Gemini protobuf).
    Framed,
    /// WebSocket text frames containing JSON.
    WebSocket,
}

/// Tool-calling dialect used for a provider/model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDialect {
    /// Mirage Tool Protocol v1 (the universal dialect).
    Mtp,
    /// Legacy XML `<tool_call>` markers.
    Xml,
    /// Gemini `function_call` fenced blocks.
    Gemini,
    /// No tool calling.
    None,
}

/// Model quirks that affect MTP prompt compilation and parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quirks {
    /// The provider's own web app injects built-in tools (search, browser,
    /// code, canvas) that may confuse the model into calling them.
    pub builtin_tool_confusion: bool,
    /// The model wraps tool blocks in markdown fences.
    pub markdown_wraps_tool_calls: bool,
    /// Append a final reminder to the MTP prompt for weak models.
    pub requires_final_reminder: bool,
    /// The model ignores system prompts; the MTP prompt must be injected
    /// into the user message instead.
    pub ignores_system_prompt: bool,
    /// Prompt verbosity level. Controls how much format explanation and
    /// how many examples are included in the MTP system prompt.
    pub prompt_style: PromptStyle,
}

/// Prompt verbosity level for the MTP system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptStyle {
    /// Short format description, one example. For strong models that
    /// reliably emit correct MTP blocks (DeepSeek, Qwen).
    Minimal,
    /// Full format description, rules, one example, tool menu. Default.
    Standard,
    /// Extra rules, multiple examples, explicit "you MUST" language.
    /// For weak models that frequently emit wrong formats (GLM, Mistral).
    Verbose,
}

impl Default for PromptStyle {
    fn default() -> Self {
        Self::Standard
    }
}

impl Default for Quirks {
    fn default() -> Self {
        Self {
            builtin_tool_confusion: true,
            markdown_wraps_tool_calls: false,
            requires_final_reminder: false,
            ignores_system_prompt: false,
            prompt_style: PromptStyle::Standard,
        }
    }
}

/// Per-provider/model capability profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Provider identifier (e.g. "deepseek").
    pub provider: String,
    /// Model identifier (e.g. "deepseek-chat").
    pub model: String,
    /// Transport used to reach the provider's internal API.
    pub transport: Transport,
    /// Tool-calling dialect. Always `Mtp` for this gateway.
    pub tool_dialect: ToolDialect,
    /// Whether OpenAI `tools` are stripped from the upstream request.
    /// Always `true` for this gateway.
    pub strip_upstream_tools: bool,
    /// Maximum number of tools to inject into the MTP prompt.
    pub max_tools: usize,
    /// Force the model to emit only one tool call per turn.
    pub force_one_tool_call: bool,
    /// Number of repair attempts for an invalid MTP block.
    pub repair_attempts: usize,
    /// Model quirks.
    pub quirks: Quirks,
}

impl ProviderProfile {
    /// Create a new profile with MTP defaults.
    pub fn new(provider: &str, model: &str, transport: Transport) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            transport,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 1,
            quirks: Quirks::default(),
        }
    }
}

/// Built-in profiles for all providers.
///
/// These use the MTP dialect universally. Transport is set from the provider
/// manifest's `StreamKind`; quirks are conservative defaults that can be
/// tuned per model after live conformance testing.
pub fn builtin_profiles() -> Vec<ProviderProfile> {
    use Transport::*;
    vec![
        // DeepSeek — strong models, minimal prompt needed.
        ProviderProfile {
            provider: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            transport: Sse,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 14,
            force_one_tool_call: false,
            repair_attempts: 1,
            quirks: Quirks {
                builtin_tool_confusion: false,
                prompt_style: PromptStyle::Minimal,
                ..Default::default()
            },
        },
        ProviderProfile {
            provider: "deepseek".to_string(),
            model: "deepseek-reasoner".to_string(),
            transport: Sse,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 14,
            force_one_tool_call: false,
            repair_attempts: 1,
            quirks: Quirks {
                builtin_tool_confusion: false,
                prompt_style: PromptStyle::Minimal,
                ..Default::default()
            },
        },
        // ChatGPT — standard prompt, no builtin confusion.
        ProviderProfile {
            provider: "chatgpt".to_string(),
            model: "gpt-4o".to_string(),
            transport: Sse,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 14,
            force_one_tool_call: false,
            repair_attempts: 1,
            quirks: Quirks {
                builtin_tool_confusion: false,
                prompt_style: PromptStyle::Standard,
                ..Default::default()
            },
        },
        ProviderProfile {
            provider: "chatgpt".to_string(),
            model: "gpt-5.6".to_string(),
            transport: Sse,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 14,
            force_one_tool_call: true,
            repair_attempts: 2,
            quirks: Quirks {
                builtin_tool_confusion: true,
                ignores_system_prompt: true,
                requires_final_reminder: true,
                prompt_style: PromptStyle::Standard,
                ..Default::default()
            },
        },
        // Claude — standard prompt.
        ProviderProfile::new("claude", "claude-opus-5", Sse),
        ProviderProfile::new("claude", "claude-sonnet-5", Sse),
        // Gemini — standard prompt, needs extra examples.
        ProviderProfile {
            provider: "gemini".to_string(),
            model: "gemini-3.6-flash".to_string(),
            transport: Framed,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 1,
            quirks: Quirks {
                builtin_tool_confusion: true,
                prompt_style: PromptStyle::Verbose,
                ..Default::default()
            },
        },
        ProviderProfile {
            provider: "gemini".to_string(),
            model: "gemini-3.1-pro".to_string(),
            transport: Framed,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 1,
            quirks: Quirks {
                builtin_tool_confusion: true,
                prompt_style: PromptStyle::Verbose,
                ..Default::default()
            },
        },
        // GLM — weak model, verbose prompt, extra reminders.
        ProviderProfile {
            provider: "glm".to_string(),
            model: "glm-5.2".to_string(),
            transport: Sse,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 2,
            quirks: Quirks {
                builtin_tool_confusion: true,
                requires_final_reminder: true,
                prompt_style: PromptStyle::Verbose,
                ..Default::default()
            },
        },
        ProviderProfile {
            provider: "glm".to_string(),
            model: "glm-4-plus".to_string(),
            transport: Sse,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 2,
            quirks: Quirks {
                builtin_tool_confusion: true,
                requires_final_reminder: true,
                prompt_style: PromptStyle::Verbose,
                ..Default::default()
            },
        },
        // Grok — standard prompt.
        ProviderProfile::new("grok", "grok-4", Sse),
        // Kimi — needs forced tool_choice, fewer tools.
        ProviderProfile {
            provider: "kimi".to_string(),
            model: "k2d6-chat".to_string(),
            transport: Framed,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 1,
            quirks: Quirks {
                builtin_tool_confusion: true,
                prompt_style: PromptStyle::Standard,
                ..Default::default()
            },
        },
        ProviderProfile {
            provider: "kimi".to_string(),
            model: "k2.6-thinking".to_string(),
            transport: Framed,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 1,
            quirks: Quirks {
                builtin_tool_confusion: true,
                prompt_style: PromptStyle::Standard,
                ..Default::default()
            },
        },
        // MetaAI — weak model, verbose prompt, extra reminders, builtin confusion.
        ProviderProfile {
            provider: "metaai".to_string(),
            model: "muse-spark".to_string(),
            transport: WebSocket,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 2,
            quirks: Quirks {
                builtin_tool_confusion: true,
                requires_final_reminder: true,
                prompt_style: PromptStyle::Verbose,
                ..Default::default()
            },
        },
        // Minimax — standard prompt.
        ProviderProfile::new("minimax", "minimax-m3", Sse),
        // Mistral — weak model, verbose prompt.
        ProviderProfile {
            provider: "mistral".to_string(),
            model: "mistral-large-latest".to_string(),
            transport: Sse,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 5,
            force_one_tool_call: true,
            repair_attempts: 2,
            quirks: Quirks {
                builtin_tool_confusion: true,
                requires_final_reminder: true,
                prompt_style: PromptStyle::Verbose,
                ..Default::default()
            },
        },
        // MiMo — standard prompt.
        ProviderProfile::new("mimo", "mimo-v2.5", Sse),
        // Qwen — strong model, minimal prompt.
        ProviderProfile {
            provider: "qwen".to_string(),
            model: "qwen3-max".to_string(),
            transport: Sse,
            tool_dialect: ToolDialect::Mtp,
            strip_upstream_tools: true,
            max_tools: 14,
            force_one_tool_call: false,
            repair_attempts: 1,
            quirks: Quirks {
                builtin_tool_confusion: false,
                prompt_style: PromptStyle::Minimal,
                ..Default::default()
            },
        },
    ]
}

/// Look up a profile by provider and model (exact match; the pipeline uses
/// the model-only lookup, this variant is for diagnostics and tests).
#[allow(dead_code)]
pub fn find_profile(provider: &str, model: &str) -> Option<ProviderProfile> {
    builtin_profiles()
        .into_iter()
        .find(|p| p.provider == provider && p.model == model)
}

/// Look up a profile by model id alone (model ids are unique across
/// providers in this gateway).
pub fn find_profile_by_model(model: &str) -> Option<ProviderProfile> {
    builtin_profiles().into_iter().find(|p| p.model == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_profiles_use_mtp() {
        for profile in builtin_profiles() {
            assert_eq!(profile.tool_dialect, ToolDialect::Mtp);
            assert!(profile.strip_upstream_tools);
        }
    }

    #[test]
    fn find_profile_returns_known() {
        let p = find_profile("deepseek", "deepseek-chat").unwrap();
        assert_eq!(p.provider, "deepseek");
        assert_eq!(p.model, "deepseek-chat");
        assert_eq!(p.transport, Transport::Sse);
    }

    #[test]
    fn find_profile_unknown_returns_none() {
        assert!(find_profile("deepseek", "nope").is_none());
        assert!(find_profile("nope", "deepseek-chat").is_none());
    }

    #[test]
    fn find_profile_by_model_lookup() {
        let p = find_profile_by_model("glm-5.2").unwrap();
        assert_eq!(p.provider, "glm");
    }

    #[test]
    fn profile_serialization_roundtrip() {
        let p = ProviderProfile::new("kimi", "k2d6-chat", Transport::Framed);
        let json = serde_json::to_string(&p).unwrap();
        let parsed: ProviderProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.provider, "kimi");
        assert_eq!(parsed.transport, Transport::Framed);
        assert_eq!(parsed.tool_dialect, ToolDialect::Mtp);
    }
}