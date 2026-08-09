use crate::models::Model;

/// All known ChatGPT models and their internal names.
///
/// The first entry is the default (`chatgpt-auto`) which routes to
/// the legacy `text-davinci-002-render-sha` (OpenAI's internal name
/// for the free-tier endpoint). Newer names (`gpt-4o`, `gpt-4`,
/// etc.) are the actual model IDs that ChatGPT's backend uses.
pub const CHATGPT_MODELS: &[(&str, &str)] = &[
    ("chatgpt-auto",   "text-davinci-002-render-sha"),
    ("gpt-4o",         "gpt-4o"),
    ("gpt-4o-mini",    "gpt-4o-mini"),
    ("gpt-4",          "gpt-4"),
    ("gpt-4-turbo",    "gpt-4-turbo"),
    ("o1",             "o1"),
    ("o1-mini",        "o1-mini"),
    ("o1-pro",         "o1-pro"),
    ("o3-mini",        "o3-mini"),
    ("gpt-4.5",        "gpt-4.5"),
    ("gpt-4.5-pro",    "gpt-4.5-pro"),
    ("gpt-5",          "gpt-5"),
    ("gpt-5.4",        "gpt-5.4"),
    ("gpt-5.5",        "gpt-5.5"),
    ("gpt-5.6",        "gpt-5.6"),
];

#[derive(Debug, Clone, Copy)]
pub struct ChatGptModelDef {
    pub id: &'static str,
    #[allow(dead_code)]
    pub internal_id: &'static str,
}

pub fn resolve_model(model_id: &str) -> Option<ChatGptModelDef> {
    CHATGPT_MODELS
        .iter()
        .find(|(id, _)| *id == model_id)
        .map(|(id, internal)| ChatGptModelDef {
            id,
            internal_id: internal,
        })
}

pub fn to_public_models() -> Vec<Model> {
    CHATGPT_MODELS
        .iter()
        .map(|(id, _)| Model {
            id: (*id).to_string(),
            object: "model".to_string(),
            created: 1735689600,
            owned_by: "openai".to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_model() {
        let m = resolve_model("chatgpt-auto").unwrap();
        assert_eq!(m.id, "chatgpt-auto");
        assert_eq!(m.internal_id, "text-davinci-002-render-sha");
    }

    #[test]
    fn resolve_unknown_model() {
        assert!(resolve_model("gpt-4-pretend").is_none());
    }

    #[test]
    fn models_include_chatgpt_auto() {
        let models = to_public_models();
        assert!(models.iter().any(|m| m.id == "chatgpt-auto"));
    }
}