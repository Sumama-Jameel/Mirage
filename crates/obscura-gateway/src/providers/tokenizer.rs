use tiktoken::get_encoding;

/// Estimate the number of tokens in `text` using the best available tokenizer
/// for the given provider and model.
///
/// Uses tiktoken BPE tokenizers where available (ChatGPT, DeepSeek).
/// Falls back to a chars/4 heuristic for providers without public tokenizer
/// support (Gemini, Kimi, GLM).
pub fn estimate_tokens(provider: &str, model: &str, text: &str) -> i32 {
    let encoding_name = resolve_encoding(provider, model);

    match encoding_name.and_then(|enc| get_encoding(enc)) {
        Some(bpe) => bpe.count(text) as i32,
        None => (text.chars().count() / 4) as i32,
    }
}

fn resolve_encoding(provider: &str, model: &str) -> Option<&'static str> {
    match provider {
        "chatgpt" => chatgpt_encoding(model),
        "deepseek" => Some("deepseek_v3"),
        _ => None,
    }
}

fn chatgpt_encoding(model: &str) -> Option<&'static str> {
    if model.contains("4o")
        || model.contains("o1")
        || model.contains("o3")
        || model.contains("o4")
        || model.contains("gpt-5")
        || model.contains("gpt-4.5")
    {
        Some("o200k_base")
    } else if model.contains("gpt-4") || model.contains("gpt-3.5") || model == "chatgpt-auto" {
        Some("cl100k_base")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chatgpt_o200k_models() {
        assert_eq!(chatgpt_encoding("gpt-4o"), Some("o200k_base"));
        assert_eq!(chatgpt_encoding("gpt-4o-mini"), Some("o200k_base"));
        assert_eq!(chatgpt_encoding("o1"), Some("o200k_base"));
        assert_eq!(chatgpt_encoding("o3-mini"), Some("o200k_base"));
        assert_eq!(chatgpt_encoding("gpt-5"), Some("o200k_base"));
    }

    #[test]
    fn test_chatgpt_cl100k_models() {
        assert_eq!(chatgpt_encoding("gpt-4"), Some("cl100k_base"));
        assert_eq!(chatgpt_encoding("gpt-4-turbo"), Some("cl100k_base"));
        assert_eq!(chatgpt_encoding("gpt-3.5-turbo"), Some("cl100k_base"));
        assert_eq!(chatgpt_encoding("chatgpt-auto"), Some("cl100k_base"));
    }

    #[test]
    fn test_chatgpt_unknown_model() {
        assert_eq!(chatgpt_encoding("unknown-model"), None);
    }

    #[test]
    fn test_estimate_tokens_deepseek() {
        let tokens = estimate_tokens("deepseek", "deepseek-chat", "Hello, world!");
        assert!(tokens > 0);
    }

    #[test]
    fn test_estimate_tokens_fallback() {
        let tokens = estimate_tokens("gemini", "gemini-3.5-flash", "Hello, world!");
        assert!(tokens > 0);
    }
}
