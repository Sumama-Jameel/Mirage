use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::error::GatewayError;
use crate::models::ToolCall;
use crate::providers::session_store::SessionStore as SharedStore;

/// Data keys stored in the shared session store.
const KEY_CONVERSATION_ID: &str = "conversation_id";
const KEY_RESPONSE_ID: &str = "response_id";
const KEY_CHOICE_ID: &str = "choice_id";
const KEY_MODEL_ID: &str = "model_id";
const KEY_TOOL_CALLS: &str = "tool_calls_stored";

/// Gemini-specific session state recovered from the shared store.
#[derive(Debug, Clone, Default)]
pub struct StoredConversation {
    pub conversation_id: String,
    pub response_id: String,
    pub choice_id: String,
    pub model_id: String,
}

/// Per-session upload cache — avoids redundant uploads of identical images.
#[derive(Clone)]
pub struct UploadCache {
    inner: Arc<Mutex<Vec<CachedUpload>>>,
    ttl: Duration,
}

#[derive(Clone)]
struct CachedUpload {
    hash: String,
    url: String,
    inserted: Instant,
}

impl UploadCache {
    pub fn new() -> Self {
        Self::with_ttl(Duration::from_secs(3600))
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
            ttl,
        }
    }

    pub async fn get(&self, hash: &str) -> Option<String> {
        let mut guard = self.inner.lock().await;
        let now = Instant::now();
        guard.retain(|e| now.duration_since(e.inserted) <= self.ttl);
        guard.iter().find(|e| e.hash == hash).map(|e| e.url.clone())
    }

    pub async fn insert(&self, hash: String, url: String) {
        let mut guard = self.inner.lock().await;
        guard.push(CachedUpload {
            hash,
            url,
            inserted: Instant::now(),
        });
    }
}

/// Gemini-specific session store.
///
/// Wraps the shared `SessionStore` and adds Gemini-specific helpers:
/// conversation state CRUD, model validation, per-session lock,
/// and upload caching.
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
            inner: SharedStore::with_data_dir(data_dir, "gemini"),
            upload_cache: UploadCache::new(),
        }
    }

    /// Acquire a session: get the stored conversation state (no lock).
    ///
    /// Returns `StoredConversation` if the session exists and is valid,
    /// or `None` if the token is unknown or expired.
    pub async fn acquire(&self, session_id: &str) -> Option<StoredConversation> {
        self.inner.acquire(session_id).await?;
        let conv_id = self.inner.get_data(session_id, KEY_CONVERSATION_ID).await?;
        let resp_id = self.inner.get_data(session_id, KEY_RESPONSE_ID).await?;
        let choice_id = self.inner.get_data(session_id, KEY_CHOICE_ID).await?;
        let model_id = self
            .inner
            .get_data(session_id, KEY_MODEL_ID)
            .await
            .unwrap_or_default();
        Some(StoredConversation {
            conversation_id: conv_id,
            response_id: resp_id,
            choice_id: choice_id,
            model_id,
        })
    }

    /// Insert or update a tracked session.
    ///
    /// Call this BEFORE `set_data` (or use this method which does both).
    /// This is the fix for the critical bug where `set_data` was called
    /// without a prior `insert`.
    pub async fn insert(
        &self,
        session_id: String,
        conv: &super::rpc::ConversationState,
        model_id: &str,
    ) {
        self.inner.insert(session_id.clone()).await;
        self.inner
            .set_data(&session_id, KEY_CONVERSATION_ID, conv.conversation_id.clone())
            .await;
        self.inner
            .set_data(&session_id, KEY_RESPONSE_ID, conv.response_id.clone())
            .await;
        self.inner
            .set_data(&session_id, KEY_CHOICE_ID, conv.choice_id.clone())
            .await;
        self.inner
            .set_data(&session_id, KEY_MODEL_ID, model_id.to_string())
            .await;
    }

    #[allow(dead_code)]
    pub async fn remove(&self, session_id: &str) {
        self.inner.remove(session_id).await;
    }

    pub async fn store_tool_calls(&self, session_id: &str, calls: &[ToolCall]) {
        self.inner.store_tool_calls(session_id, calls).await;
        self.inner
            .set_data(session_id, KEY_TOOL_CALLS, "1".to_string())
            .await;
    }

    pub async fn get_tool_call(&self, session_id: &str, call_id: &str) -> Option<ToolCall> {
        self.inner.get_tool_call(session_id, call_id).await
    }

    /// Validate that the model hasn't changed mid-conversation.
    pub fn ensure_model_matches(
        requested_model: &str,
        tracked_model: &str,
    ) -> Result<(), GatewayError> {
        if tracked_model.is_empty() {
            return Ok(());
        }
        if requested_model != tracked_model {
            return Err(GatewayError::BadRequest(format!(
                "session was created with model '{}', but request asks for '{}'",
                tracked_model, requested_model
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
    use crate::providers::gemini::rpc::ConversationState;

    #[tokio::test]
    async fn acquire_returns_none_for_unknown_token() {
        let store = SessionStore::new();
        let result = store.acquire("unknown-token").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn insert_and_acquire_roundtrip() {
        let store = SessionStore::new();
        let conv = ConversationState {
            conversation_id: "c_test".to_string(),
            response_id: "r_test".to_string(),
            choice_id: "ch_test".to_string(),
        };
        store.insert("tok1".to_string(), &conv, "gemini-3.5-flash").await;

        let stored = store.acquire("tok1").await.unwrap();
        assert_eq!(stored.conversation_id, "c_test");
        assert_eq!(stored.response_id, "r_test");
        assert_eq!(stored.choice_id, "ch_test");
        assert_eq!(stored.model_id, "gemini-3.5-flash");
    }

    #[tokio::test]
    async fn ensure_model_matches_accepts_same() {
        assert!(SessionStore::ensure_model_matches("model-a", "model-a").is_ok());
    }

    #[tokio::test]
    async fn ensure_model_matches_rejects_different() {
        let err = SessionStore::ensure_model_matches("model-a", "model-b").unwrap_err();
        assert!(err.to_string().contains("model"));
        assert!(err.to_string().contains("model-a"));
    }

    #[tokio::test]
    async fn ensure_model_matches_accepts_empty_tracked() {
        assert!(SessionStore::ensure_model_matches("model-a", "").is_ok());
    }

    #[tokio::test]
    async fn store_and_retrieve_tool_call() {
        let store = SessionStore::new();
        store.insert("tok2".to_string(), &ConversationState::default(), "m").await;

        let call = ToolCall {
            id: "call_1".to_string(),
            r#type: "function".to_string(),
            function: crate::models::FunctionCall {
                name: "test_fn".to_string(),
                arguments: r#"{"arg":1}"#.to_string(),
            },
        };
        store.store_tool_calls("tok2", &[call.clone()]).await;

        let retrieved = store.get_tool_call("tok2", "call_1").await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().function.name, "test_fn");
    }

    #[tokio::test]
    async fn upload_cache_insert_and_get() {
        let cache = UploadCache::new();
        cache.insert("abc123".to_string(), "http://url".to_string()).await;
        let result = cache.get("abc123").await;
        assert_eq!(result, Some("http://url".to_string()));
    }

    #[tokio::test]
    async fn upload_cache_unknown_hash() {
        let cache = UploadCache::new();
        assert!(cache.get("nonexistent").await.is_none());
    }
}
