use crate::models::Model;

#[derive(Debug, Clone, Copy)]
pub struct ClaudeModelDef {
    pub id: &'static str,
    #[allow(dead_code)]
    pub internal_id: &'static str,
    pub supports_thinking: bool,
    pub supports_search: bool,
    pub supports_vision: bool,
    pub supports_tools: bool,
}

/// Current-generation Claude models support all features.
const fn current_gen(id: &'static str, internal_id: &'static str) -> ClaudeModelDef {
    ClaudeModelDef {
        id,
        internal_id,
        supports_thinking: true,
        supports_search: true,
        supports_vision: true,
        supports_tools: true,
    }
}

/// Legacy models lack thinking but support search, vision, and tools.
const fn legacy_gen(id: &'static str, internal_id: &'static str) -> ClaudeModelDef {
    ClaudeModelDef {
        id,
        internal_id,
        supports_thinking: false,
        supports_search: true,
        supports_vision: true,
        supports_tools: true,
    }
}

/// Claude 3 Opus is the oldest model — supports vision and tools but not thinking or search.
const fn claude3_opus(id: &'static str, internal_id: &'static str) -> ClaudeModelDef {
    ClaudeModelDef {
        id,
        internal_id,
        supports_thinking: false,
        supports_search: false,
        supports_vision: true,
        supports_tools: true,
    }
}

pub fn resolve_model(model_id: &str) -> Option<ClaudeModelDef> {
    match model_id {
        "claude-fable-5" => Some(current_gen("claude-fable-5", "claude-fable-5")),
        "claude-sonnet-5" => Some(current_gen("claude-sonnet-5", "claude-sonnet-5")),
        "claude-opus-4-8" => Some(current_gen("claude-opus-4-8", "claude-opus-4-8")),
        "claude-haiku-4-5-20251001" => Some(current_gen("claude-haiku-4-5-20251001", "claude-haiku-4-5-20251001")),
        "claude-haiku-4-5" => Some(current_gen("claude-haiku-4-5", "claude-haiku-4-5")),
        "claude-sonnet-4-6" => Some(legacy_gen("claude-sonnet-4-6", "claude-sonnet-4-6")),
        "claude-opus-4-7" => Some(legacy_gen("claude-opus-4-7", "claude-opus-4-7")),
        "claude-sonnet-4-5" => Some(legacy_gen("claude-sonnet-4-5", "claude-sonnet-4-5")),
        "claude-sonnet-4-5-20250929" => Some(legacy_gen("claude-sonnet-4-5-20250929", "claude-sonnet-4-5-20250929")),
        "claude-3-opus-20240229" => Some(claude3_opus("claude-3-opus-20240229", "claude-3-opus-20240229")),
        _ => None,
    }
}

pub fn to_public_models() -> Vec<Model> {
    // All known Claude model IDs.
    // Keep this list in sync with `resolve_model`.
    const KNOWN: &[&str] = &[
        "claude-fable-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
        "claude-haiku-4-5-20251001",
        "claude-haiku-4-5",
        "claude-sonnet-4-6",
        "claude-opus-4-7",
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-20250929",
        "claude-3-opus-20240229",
    ];
    KNOWN
        .iter()
        .map(|id| Model {
            id: id.to_string(),
            object: "model".to_string(),
            created: 1735689600,
            owned_by: "anthropic".to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_sonnet_5() {
        let m = resolve_model("claude-sonnet-5").unwrap();
        assert_eq!(m.id, "claude-sonnet-5");
    }

    #[test]
    fn resolve_fable_5() {
        let m = resolve_model("claude-fable-5").unwrap();
        assert_eq!(m.id, "claude-fable-5");
    }

    #[test]
    fn resolve_opus_4_8() {
        let m = resolve_model("claude-opus-4-8").unwrap();
        assert_eq!(m.id, "claude-opus-4-8");
    }

    #[test]
    fn resolve_known_model() {
        let m = resolve_model("claude-sonnet-4-6").unwrap();
        assert_eq!(m.id, "claude-sonnet-4-6");
        assert_eq!(m.internal_id, "claude-sonnet-4-6");
    }

    #[test]
    fn resolve_haiku_model() {
        let m = resolve_model("claude-haiku-4-5").unwrap();
        assert_eq!(m.internal_id, "claude-haiku-4-5");
    }

    #[test]
    fn resolve_unknown_model() {
        assert!(resolve_model("gpt-4").is_none());
    }

    #[test]
    fn models_include_all() {
        let models = to_public_models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"claude-sonnet-5"));
        assert!(ids.contains(&"claude-fable-5"));
        assert!(ids.contains(&"claude-opus-4-8"));
        assert!(ids.contains(&"claude-haiku-4-5"));
        assert!(ids.contains(&"claude-sonnet-4-6"));
        assert!(ids.contains(&"claude-opus-4-7"));
        assert!(ids.contains(&"claude-sonnet-4-5"));
        assert!(ids.contains(&"claude-sonnet-4-5-20250929"));
        assert!(ids.contains(&"claude-3-opus-20240229"));
    }
}
