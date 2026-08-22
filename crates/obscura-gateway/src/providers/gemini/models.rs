use crate::models::Model;

/// Model-specific header used by Google's internal Gemini API to select the model.
/// The `x-goog-ext-525001261-jspb` value is a JSON-encoded array.
pub struct GeminiModelDef {
    pub id: &'static str,
    pub owned_by: &'static str,
    /// Value for the `x-goog-ext-525001261-jspb` request header.
    /// None for models that are selected purely by the payload mode field
    /// (verified against the live gemini.google.com client: non-Pro models
    /// carry no model header, field 79 selects the model).
    pub model_header: Option<&'static str>,
    /// MODE_CATEGORY for the `[79]` array slot in StreamGenerate payload.
    /// 1=FAST, 2=THINKING, 3=PRO, 4=AUTO, 5=FAST_DYNAMIC_THINKING, 6=FLASH_LITE
    pub mode: u32,
    pub supports_thinking: bool,
    pub supports_search: bool,
    pub supports_vision: bool,
    pub supports_tools: bool,
}

pub const GEMINI_MODELS: &[GeminiModelDef] = &[
    GeminiModelDef {
        id: "gemini-3.6-flash",
        owned_by: "google",
        model_header: None,
        mode: 1,
        supports_thinking: true,
        supports_search: true,
        supports_vision: true,
        supports_tools: true,
    },
    GeminiModelDef {
        id: "gemini-3.5-flash",
        owned_by: "google",
        model_header: Some(r#"[1,null,null,null,"56fdd199312815e2",null,null,0,[4,5,6,8],null,null,2,null,null,1,1,"09D681E7-26F2-4A94-A465-38386B7AB93B"]"#),
        mode: 1,
        supports_thinking: true,
        supports_search: true,
        supports_vision: true,
        supports_tools: true,
    },
    GeminiModelDef {
        id: "gemini-3.1-pro",
        owned_by: "google",
        model_header: Some(r#"[1,null,null,null,"e6fa609c3fa255c0",null,null,0,[4,5,6,8],null,null,2,null,null,3,1,"09D681E7-26F2-4A94-A465-38386B7AB93B"]"#),
        mode: 3,
        supports_thinking: true,
        supports_search: true,
        supports_vision: true,
        supports_tools: true,
    },
    GeminiModelDef {
        id: "gemini-3.5-flash-lite",
        owned_by: "google",
        model_header: None,
        mode: 6,
        supports_thinking: false,
        supports_search: true,
        supports_vision: true,
        supports_tools: true,
    },
    GeminiModelDef {
        id: "gemini-3.1-flash-lite",
        owned_by: "google",
        model_header: Some(r#"[1,null,null,null,"8c46e95b1a07cecc",null,null,0,[4,5,6,8],null,null,2,null,null,6,1,"09D681E7-26F2-4A94-A465-38386B7AB93B"]"#),
        mode: 6,
        supports_thinking: false,
        supports_search: true,
        supports_vision: true,
        supports_tools: true,
    },
    GeminiModelDef {
        id: "gemini-deep-research",
        owned_by: "google",
        model_header: Some(r#"[1,null,null,null,"cd472a54d2abba7e"]"#),
        mode: 2,
        supports_thinking: true,
        supports_search: true,
        supports_vision: false,
        supports_tools: false,
    },
];

pub fn resolve_model(model_id: &str) -> Option<&'static GeminiModelDef> {
    GEMINI_MODELS.iter().find(|m| m.id == model_id)
}

pub fn to_public_models() -> Vec<Model> {
    GEMINI_MODELS
        .iter()
        .map(|m| Model {
            id: m.id.to_string(),
            object: "model".to_string(),
            created: 1_700_000_000,
            owned_by: m.owned_by.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_models() {
        assert!(resolve_model("gemini-3.6-flash").is_some());
        assert!(resolve_model("gemini-3.5-flash").is_some());
        assert!(resolve_model("gemini-3.5-flash-lite").is_some());
        assert!(resolve_model("gemini-3.1-pro").is_some());
        assert!(resolve_model("gemini-3.1-flash-lite").is_some());
    }

    #[test]
    fn resolve_unknown_model() {
        assert!(resolve_model("gemini-2.5-flash").is_none());
        assert!(resolve_model("nonexistent").is_none());
    }

    #[test]
    fn public_models_count_matches() {
        let pub_models = to_public_models();
        assert_eq!(pub_models.len(), GEMINI_MODELS.len());
        assert!(pub_models.iter().any(|m| m.id == "gemini-3.5-flash"));
    }
}
