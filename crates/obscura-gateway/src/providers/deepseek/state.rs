//! DeepSeek session state store.
//!
//! Thin wrapper around the shared [`SessionStore`](crate::providers::session_store::SessionStore)
//! that adds DeepSeek-specific helpers for model-type tracking, the real
//! DeepSeek chat session id, and message identifiers.
//!
//! The store is keyed by an opaque *branch token* minted per completed turn,
//! not by the raw DeepSeek chat session id. A branch token maps to the real
//! `chat_session_id` plus the assistant message that ended that turn. Because
//! every completion mints a fresh token, two concurrent continuations from
//! the same parent each get their own continuable branch instead of sharing a
//! single last-writer-wins tip (ChatGPT web message-tree semantics).
//!
//! Provider-specific data is stored as key-value pairs in the shared store:
//!
//! | Key | Value |
//! |-----|-------|
//! | `"chat_session_id"` | Real DeepSeek chat session id |
//! | `"model_type"` | DeepSeek internal model type |
//! | `"last_assistant_message_id"` | Last message id for continuation |
//!
//! The [`UploadCache`] is kept here (not in the shared store) because the
//! upload endpoint is DeepSeek-specific.

use std::path::PathBuf;

use crate::error::GatewayError;
use crate::models::ToolCall;
use crate::providers::session_store::SessionStore as SharedStore;

use super::upload::UploadCache;

const KEY_CHAT_SESSION_ID: &str = "chat_session_id";
const KEY_MODEL_TYPE: &str = "model_type";
const KEY_LAST_MSG_ID: &str = "last_assistant_message_id";

/// In-memory store of DeepSeek sessions that can be continued.
#[derive(Clone)]
pub struct SessionStore {
    inner: SharedStore,
    pub upload_cache: UploadCache,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: SharedStore::new(),
            upload_cache: UploadCache::new(),
        }
    }

    /// Create a store with optional disk persistence.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            inner: SharedStore::with_data_dir(data_dir, "deepseek"),
            upload_cache: UploadCache::new(),
        }
    }

    /// Acquire continuation metadata for a branch token.
    /// Returns `(chat_session_id, last_message_id, model_type)`.
    /// Returns `None` if the session is unknown or expired.
    pub async fn acquire(
        &self,
        session_id: &str,
    ) -> Option<(String, i64, String)> {
        self.inner.acquire(session_id).await?;
        let chat_session_id = self
            .inner
            .get_data(session_id, KEY_CHAT_SESSION_ID)
            .await
            .unwrap_or_default();
        let msg_id: i64 = self
            .inner
            .get_data(session_id, KEY_LAST_MSG_ID)
            .await
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let model = self
            .inner
            .get_data(session_id, KEY_MODEL_TYPE)
            .await
            .unwrap_or_default();
        Some((chat_session_id, msg_id, model))
    }

    /// Insert or update a tracked branch token.
    pub async fn insert(
        &self,
        session_id: String,
        model_type: String,
        chat_session_id: String,
        assistant_message_id: i64,
    ) {
        self.inner.insert(session_id.clone()).await;
        self.inner
            .set_data(&session_id, KEY_CHAT_SESSION_ID, chat_session_id)
            .await;
        self.inner
            .set_data(&session_id, KEY_MODEL_TYPE, model_type)
            .await;
        self.inner
            .set_data(&session_id, KEY_LAST_MSG_ID, assistant_message_id.to_string())
            .await;
    }

    /// Remove a session explicitly, e.g. after a continuation failure.
    #[allow(dead_code)]
    pub async fn remove(&self, session_id: &str) {
        self.inner.remove(session_id).await;
    }

    /// Store tool calls emitted by the assistant in a session.
    pub async fn store_tool_calls(&self, session_id: &str, calls: &[ToolCall]) {
        self.inner.store_tool_calls(session_id, calls).await;
    }

    /// Retrieve a previously stored tool call by id.
    pub async fn get_tool_call(&self, session_id: &str, call_id: &str) -> Option<ToolCall> {
        self.inner.get_tool_call(session_id, call_id).await
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate that a requested model matches a tracked session's model.
pub fn ensure_model_matches(
    requested_model: &str,
    tracked_model: &str,
) -> Result<(), GatewayError> {
    use crate::providers::session_store::ensure_model_matches as shared_check;
    shared_check(requested_model, tracked_model)
}

/// Maps a public model id to DeepSeek's internal `model_type` wire value.
#[allow(dead_code)]
fn resolve_public_model_type(model_id: &str) -> Option<&'static str> {
    match model_id {
        "deepseek-chat" | "deepseek-instant" => Some("default"),
        "deepseek-reasoner" | "deepseek-expert" => Some("expert"),
        "deepseek-vision" => Some("vision"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn branch_token_roundtrips_chat_session_and_message() {
        let store = SessionStore::new();

        // A completed turn is stored under a fresh branch token and resolves
        // back to the real DeepSeek chat session id plus its message id.
        store
            .insert(
                "deepseek_branch-a".to_string(),
                "default".to_string(),
                "chat-1".to_string(),
                42,
            )
            .await;
        let (chat_id, msg_id, model) = store.acquire("deepseek_branch-a").await.unwrap();
        assert_eq!(chat_id, "chat-1");
        assert_eq!(msg_id, 42);
        assert_eq!(model, "default");
    }

    #[tokio::test]
    async fn sibling_branches_keep_distinct_tips() {
        let store = SessionStore::new();

        // Two concurrent completions from the same parent mint two tokens;
        // each resolves to its own message id, not a shared last-writer tip.
        store
            .insert(
                "deepseek_branch-a".to_string(),
                "default".to_string(),
                "chat-1".to_string(),
                101,
            )
            .await;
        store
            .insert(
                "deepseek_branch-b".to_string(),
                "default".to_string(),
                "chat-1".to_string(),
                102,
            )
            .await;

        let (_, msg_a, _) = store.acquire("deepseek_branch-a").await.unwrap();
        let (_, msg_b, _) = store.acquire("deepseek_branch-b").await.unwrap();
        assert_eq!(msg_a, 101);
        assert_eq!(msg_b, 102);
    }

    #[tokio::test]
    async fn unknown_branch_token_is_not_acquirable() {
        let store = SessionStore::new();
        assert!(store.acquire("deepseek_ghost").await.is_none());
    }
}
