use crate::models::Model;

/// GLM model definitions for the free `chat.z.ai` web UI.
///
/// Tuple: (public_id, internal_id, supports_thinking, supports_vision,
///         supports_search, supports_tool_streaming)
///
/// `internal_id` is the model name used inside the `chat.z.ai` request payload.
/// It must match the values returned by the site's `/api/models` endpoint and
/// observed in live traffic. These values are verified against the user's
/// captured payload and open-source reverse-engineering projects.
///
/// `supports_tool_streaming` indicates whether the model accepts the
/// `tool_stream: true` flag. Z.AI's official docs specify this works for
/// `glm-4.6`, `glm-4.7`, and the `glm-5` family. When tools are present in the
/// request and the model supports streaming tool calls, the flag MUST be set
/// or the model produces degenerate tool calling (wrong tools in a loop, or
/// none at all). See Z.AI Stream Tool Call docs.
pub const GLM_MODELS: &[(&str, &str, bool, bool, bool, bool)] = &[
    // GLM-5 generation (chat.z.ai free web UI)
    ("glm-5.2", "glm-5.2", true, false, true, true),
    ("glm-5.1", "GLM-5.1", true, false, true, true),
    ("glm-5", "GLM-5", true, false, true, true),
    ("glm-5-turbo", "GLM-5-Turbo", true, false, true, true),
    ("glm-5v-turbo", "GLM-5v-Turbo", true, true, true, true),
    // GLM-4 generation (chat.z.ai free web UI)
    ("glm-4.7", "glm-4.7", true, false, true, true),
    ("glm-4.6v", "glm-4.6v", true, true, true, true),
    ("glm-4.6", "glm-4.6", true, false, true, true),
    ("glm-4-plus", "glm-4-plus", false, false, true, false),
    ("glm-4-zero", "glm-4-zero", true, false, true, false),
    ("glm-4-think", "glm-4-think", true, false, true, false),
    ("glm-4-deepresearch", "glm-4-deepresearch", false, false, true, false),
    // Variants and lighter-weight models verified from /api/models endpoint
    ("glm-4.7-flashx", "GLM-4.7-FlashX", true, false, true, true),
    ("glm-4.7-long", "GLM-4.7-Long", true, false, true, true),
    ("glm-4.5-air", "GLM-4.5-Air", false, false, true, false),
    ("glm-4.5-thinking", "GLM-4.5-Thinking", true, false, true, false),
];

#[derive(Debug, Clone)]
pub struct GlmModelDef {
    pub id: String,
    pub internal_id: String,
    pub supports_thinking: bool,
    pub supports_vision: bool,
    pub supports_search: bool,
    pub supports_tool_streaming: bool,
    pub supports_tools: bool,
}

pub fn resolve_model(model_id: &str) -> Option<GlmModelDef> {
    GLM_MODELS
        .iter()
        .find(|(id, _, _, _, _, _)| *id == model_id)
        .map(
            |(id, internal, thinking, vision, search, tool_stream)| GlmModelDef {
                id: (*id).to_string(),
                internal_id: (*internal).to_string(),
                supports_thinking: *thinking,
                supports_vision: *vision,
                supports_search: *search,
                supports_tool_streaming: *tool_stream,
                supports_tools: true,
            },
        )
}

pub fn to_public_models() -> Vec<Model> {
    GLM_MODELS
        .iter()
        .map(|(id, _, _, _, _, _)| Model {
            id: (*id).to_string(),
            object: "model".to_string(),
            created: 1_735_689_600,
            owned_by: "zai".to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_glm_5_2() {
        let m = resolve_model("glm-5.2").unwrap();
        assert_eq!(m.id, "glm-5.2");
        assert_eq!(m.internal_id, "glm-5.2");
        assert!(m.supports_thinking);
        assert!(!m.supports_vision);
        assert!(m.supports_search);
        assert!(m.supports_tool_streaming);
    }

    #[test]
    fn resolve_glm_5_1() {
        let m = resolve_model("glm-5.1").unwrap();
        assert_eq!(m.id, "glm-5.1");
        assert_eq!(m.internal_id, "GLM-5.1");
        assert!(m.supports_tool_streaming);
    }

    #[test]
    fn resolve_glm_5v_turbo() {
        let m = resolve_model("glm-5v-turbo").unwrap();
        assert_eq!(m.id, "glm-5v-turbo");
        assert_eq!(m.internal_id, "GLM-5v-Turbo");
        assert!(m.supports_vision);
        assert!(m.supports_tool_streaming);
    }

    #[test]
    fn resolve_glm_4_plus_no_tool_stream() {
        // Older glm-4-plus does NOT support tool streaming per Z.AI docs.
        let m = resolve_model("glm-4-plus").unwrap();
        assert!(!m.supports_tool_streaming);
    }

    #[test]
    fn resolve_unknown_model() {
        assert!(resolve_model("gpt-4").is_none());
    }

    #[test]
    fn models_include_all() {
        let models = to_public_models();
        for (id, _, _, _, _, _) in GLM_MODELS {
            assert!(
                models.iter().any(|m| m.id == *id),
                "missing model: {}",
                id
            );
        }
    }
}
