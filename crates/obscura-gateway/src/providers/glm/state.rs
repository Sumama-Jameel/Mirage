use std::path::PathBuf;

use crate::error::GatewayError;
use crate::models::ToolCall;
use crate::providers::session_store::SessionStore as SharedStore;

const KEY_MODEL_ID: &str = "model_id";

#[derive(Clone)]
pub struct SessionStore {
    inner: SharedStore,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: SharedStore::new(),
        }
    }

    /// Create a store with optional disk persistence.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            inner: SharedStore::with_data_dir(data_dir, "glm"),
        }
    }

    /// Acquire model id for an existing `chat_id` (no lock).
    pub async fn acquire(&self, chat_id: &str) -> Option<String> {
        self.inner.acquire(chat_id).await?;
        let model_id = self
            .inner
            .get_data(chat_id, KEY_MODEL_ID)
            .await
            .unwrap_or_default();
        Some(model_id)
    }

    /// Store a chat_id → model_id mapping.
    pub async fn insert(&self, chat_id: String, model_id: &str) {
        self.inner.insert(chat_id.clone()).await;
        self.inner
            .set_data(&chat_id, KEY_MODEL_ID, model_id.to_string())
            .await;
    }

    pub async fn get_model(&self, chat_id: &str) -> Option<String> {
        self.inner.get_data(chat_id, KEY_MODEL_ID).await
    }

    /// Store tool calls emitted by the assistant in a chat.
    ///
    /// Called after a turn produces tool calls so a later `role: "tool"`
    /// message in the same chat can look up the call by `tool_call_id` and
    /// reconstruct the prior tool invocation. This is the foundation of
    /// the multi-turn agent loop and mirrors `deepseek/state.rs` and
    /// `gemini/state.rs` exactly.
    pub async fn store_tool_calls(&self, chat_id: &str, calls: &[ToolCall]) {
        self.inner.store_tool_calls(chat_id, calls).await;
    }

    /// Retrieve a previously stored tool call by its `id`.
    ///
    /// Returns `None` if the call is not stored, the chat is unknown, or
    /// the chat has been evicted by TTL/LRU. Used by
    /// `crate::providers::glm::mod::handle_tool_results` to format prior
    /// tool results back into the prompt.
    pub async fn get_tool_call(&self, chat_id: &str, call_id: &str) -> Option<ToolCall> {
        self.inner.get_tool_call(chat_id, call_id).await
    }

    /// Explicitly remove a chat (e.g. on a continuation failure).
    #[allow(dead_code)]
    pub async fn remove(&self, chat_id: &str) {
        self.inner.remove(chat_id).await;
    }

    pub fn ensure_model_matches(
        requested_model: &str,
        tracked_model: &str,
    ) -> Result<(), GatewayError> {
        if tracked_model.is_empty() {
            return Ok(());
        }
        if requested_model != tracked_model {
            return Err(GatewayError::BadRequest(format!(
                "chat was started with model '{tracked_model}', but request asks for '{requested_model}'"
            )));
        }
        Ok(())
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_and_acquire_chat() {
        let store = SessionStore::new();
        store.insert("chat-123".to_string(), "glm-5.2").await;
        let model = store.acquire("chat-123").await.unwrap();
        assert_eq!(model, "glm-5.2");
        assert!(SessionStore::ensure_model_matches("glm-5.2", &model).is_ok());
    }

    #[tokio::test]
    async fn model_mismatch_fails() {
        let store = SessionStore::new();
        store.insert("chat-123".to_string(), "glm-5.2").await;
        let model = store.acquire("chat-123").await.unwrap();
        assert!(SessionStore::ensure_model_matches("glm-5.1", &model).is_err());
    }

    #[tokio::test]
    async fn tool_call_roundtrip() {
        let store = SessionStore::new();
        store.insert("chat-1".to_string(), "glm-5.2").await;

        let call = ToolCall {
            id: "call_abc".to_string(),
            r#type: "function".to_string(),
            function: crate::models::FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Paris"}"#.to_string(),
            },
        };
        store.store_tool_calls("chat-1", &[call.clone()]).await;

        let retrieved = store.get_tool_call("chat-1", "call_abc").await.unwrap();
        assert_eq!(retrieved.id, "call_abc");
        assert_eq!(retrieved.function.name, "get_weather");
        assert_eq!(retrieved.function.arguments, r#"{"city":"Paris"}"#);

        // Different call id: not stored.
        assert!(store.get_tool_call("chat-1", "missing").await.is_none());
        // Different chat: not stored.
        assert!(store.get_tool_call("chat-2", "call_abc").await.is_none());
    }

    #[tokio::test]
    async fn store_multiple_tool_calls_in_same_chat() {
        let store = SessionStore::new();
        store.insert("chat-2".to_string(), "glm-5.2").await;

        let calls = vec![
            ToolCall {
                id: "c1".to_string(),
                r#type: "function".to_string(),
                function: crate::models::FunctionCall {
                    name: "fn_a".to_string(),
                    arguments: "{}".to_string(),
                },
            },
            ToolCall {
                id: "c2".to_string(),
                r#type: "function".to_string(),
                function: crate::models::FunctionCall {
                    name: "fn_b".to_string(),
                    arguments: r#"{"x":1}"#.to_string(),
                },
            },
        ];
        store.store_tool_calls("chat-2", &calls).await;

        assert_eq!(
            store.get_tool_call("chat-2", "c1").await.unwrap().function.name,
            "fn_a"
        );
        assert_eq!(
            store.get_tool_call("chat-2", "c2").await.unwrap().function.name,
            "fn_b"
        );
    }

    #[tokio::test]
    async fn remove_chat_drops_tool_calls() {
        let store = SessionStore::new();
        store.insert("chat-3".to_string(), "glm-5.2").await;
        let call = ToolCall {
            id: "c1".to_string(),
            r#type: "function".to_string(),
            function: crate::models::FunctionCall {
                name: "fn".to_string(),
                arguments: "{}".to_string(),
            },
        };
        store.store_tool_calls("chat-3", &[call]).await;
        assert!(store.get_tool_call("chat-3", "c1").await.is_some());

        store.remove("chat-3").await;
        assert!(store.acquire("chat-3").await.is_none());
        assert!(store.get_tool_call("chat-3", "c1").await.is_none());
    }

    #[tokio::test]
    async fn get_tool_call_before_insert_returns_none() {
        let store = SessionStore::new();
        assert!(store.get_tool_call("nonexistent", "any").await.is_none());
    }
}
