use crate::models::Model;

#[derive(Debug, Clone)]
pub struct KimiModelDef {
    pub id: String,
    pub kimiplus_id: String,
    pub use_search: bool,
    pub use_research: bool,
    pub use_math: bool,
    pub is_thinking: bool,
    pub supports_vision: bool,
    pub supports_tools: bool,
}

pub fn resolve_model(model_id: &str) -> Option<KimiModelDef> {
    match model_id {
        // Flagship — launched July 16, 2026, ~2.8T MoE, 1M context, always thinks
        "kimi-k3" => Some(KimiModelDef {
            id: "kimi-k3".to_string(),
            kimiplus_id: "kimi".to_string(),
            use_search: false,
            use_research: false,
            use_math: false,
            is_thinking: true,
            supports_vision: true,
            supports_tools: true,
        }),
        // Current-gen coding specialist
        "kimi-k2.7-code" => Some(KimiModelDef {
            id: "kimi-k2.7-code".to_string(),
            kimiplus_id: "kimi".to_string(),
            use_search: false,
            use_research: false,
            use_math: false,
            is_thinking: true,
            supports_vision: true,
            supports_tools: true,
        }),
        "kimi-k2.7-code-highspeed" => Some(KimiModelDef {
            id: "kimi-k2.7-code-highspeed".to_string(),
            kimiplus_id: "kimi".to_string(),
            use_search: false,
            use_research: false,
            use_math: false,
            is_thinking: true,
            supports_vision: true,
            supports_tools: true,
        }),
        // Previous-gen flagships (still active, sunsetting)
        "kimi-k2.6" => Some(KimiModelDef {
            id: "kimi-k2.6".to_string(),
            kimiplus_id: "kimi".to_string(),
            use_search: false,
            use_research: false,
            use_math: false,
            is_thinking: false,
            supports_vision: true,
            supports_tools: true,
        }),
        "kimi-k2.5" => Some(KimiModelDef {
            id: "kimi-k2.5".to_string(),
            kimiplus_id: "kimi".to_string(),
            use_search: false,
            use_research: false,
            use_math: false,
            is_thinking: false,
            supports_vision: true,
            supports_tools: true,
        }),
        // Special-purpose
        "kimi-search" => Some(KimiModelDef {
            id: "kimi-search".to_string(),
            kimiplus_id: "kimi".to_string(),
            use_search: true,
            use_research: false,
            use_math: false,
            is_thinking: false,
            supports_vision: false,
            supports_tools: false,
        }),
        "kimi-research" => Some(KimiModelDef {
            id: "kimi-research".to_string(),
            kimiplus_id: "kimi".to_string(),
            use_search: false,
            use_research: true,
            use_math: false,
            is_thinking: false,
            supports_vision: false,
            supports_tools: false,
        }),
        // Deprecated (kept for backward compat; will be removed in a future release)
        id @ ("kimi-k2" | "kimi-k2-thinking" | "kimi-k2-turbo-preview" | "kimi-k1") => {
            tracing::warn!(model = %id, "Kimi model '{}' is deprecated and may stop working", id);
            let (kimiplus_id, vision, tools) = match id {
                "kimi-k1" => ("crm40ee9e5jvhsn7ptcg", false, false),
                _ => ("kimi", true, true),
            };
            Some(KimiModelDef {
                id: id.to_string(),
                kimiplus_id: kimiplus_id.to_string(),
                use_search: false,
                use_research: false,
                use_math: false,
                is_thinking: id == "kimi-k2-thinking",
                supports_vision: vision,
                supports_tools: tools,
            })
        }
        _ if model_id.len() == 20 && model_id.chars().all(|c| c.is_ascii_alphanumeric()) => {
            Some(KimiModelDef {
                id: model_id.to_string(),
                kimiplus_id: model_id.to_string(),
                use_search: false,
                use_research: false,
                use_math: false,
                is_thinking: false,
                supports_vision: false,
                supports_tools: false,
            })
        }
        _ => None,
    }
}

pub fn to_public_models() -> Vec<Model> {
    const KNOWN: &[&str] = &[
        "kimi-k3",
        "kimi-k2.7-code",
        "kimi-k2.7-code-highspeed",
        "kimi-k2.6",
        "kimi-k2.5",
        "kimi-search",
        "kimi-research",
    ];
    KNOWN
        .iter()
        .map(|id| Model {
            id: id.to_string(),
            object: "model".to_string(),
            created: 1735689600,
            owned_by: "moonshot".to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_k3() {
        let m = resolve_model("kimi-k3").unwrap();
        assert_eq!(m.id, "kimi-k3");
        assert!(m.is_thinking);
        assert!(m.supports_vision);
        assert!(m.supports_tools);
    }

    #[test]
    fn resolve_k2_7_code() {
        let m = resolve_model("kimi-k2.7-code").unwrap();
        assert_eq!(m.id, "kimi-k2.7-code");
        assert!(m.is_thinking);
    }

    #[test]
    fn resolve_k2_6() {
        let m = resolve_model("kimi-k2.6").unwrap();
        assert_eq!(m.id, "kimi-k2.6");
        assert_eq!(m.kimiplus_id, "kimi");
        assert!(!m.use_search);
    }

    #[test]
    fn resolve_k2_5() {
        let m = resolve_model("kimi-k2.5").unwrap();
        assert_eq!(m.id, "kimi-k2.5");
        assert_eq!(m.kimiplus_id, "kimi");
    }

    #[test]
    fn resolve_deprecated_k2() {
        let m = resolve_model("kimi-k2").unwrap();
        assert_eq!(m.id, "kimi-k2");
        assert_eq!(m.kimiplus_id, "kimi");
    }

    #[test]
    fn resolve_deprecated_k1() {
        let m = resolve_model("kimi-k1").unwrap();
        assert_eq!(m.id, "kimi-k1");
        assert_eq!(m.kimiplus_id, "crm40ee9e5jvhsn7ptcg");
    }

    #[test]
    fn resolve_search_model() {
        let m = resolve_model("kimi-search").unwrap();
        assert!(m.use_search);
    }

    #[test]
    fn resolve_research_model() {
        let m = resolve_model("kimi-research").unwrap();
        assert!(m.use_research);
    }

    #[test]
    fn resolve_agent_id() {
        let m = resolve_model("abc123def456ghi789j0").unwrap();
        assert_eq!(m.kimiplus_id, "abc123def456ghi789j0");
    }

    #[test]
    fn resolve_unknown_model() {
        assert!(resolve_model("gpt-4").is_none());
    }

    #[test]
    fn models_include_current() {
        let models = to_public_models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"kimi-k3"));
        assert!(ids.contains(&"kimi-k2.7-code"));
        assert!(ids.contains(&"kimi-k2.7-code-highspeed"));
        assert!(ids.contains(&"kimi-k2.6"));
        assert!(ids.contains(&"kimi-k2.5"));
        assert!(ids.contains(&"kimi-search"));
        assert!(ids.contains(&"kimi-research"));
        assert_eq!(ids.len(), 7);
    }
}
