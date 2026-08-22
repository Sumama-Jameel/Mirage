use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Tracks a Qwen chat session for multi-turn conversation continuity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QwenSessionState {
    /// The upstream chat_id returned by POST /api/v2/chats/new.
    pub chat_id: String,
    /// The upstream model used to create this session.
    pub model: String,
    #[serde(default)]
    pub tool_calls: HashMap<String, crate::models::ToolCall>,
    /// The id of the last assistant message, from the `response.created`
    /// SSE event. The next turn links its new user message to this id via
    /// `parent_id`, which is how the server reconstructs conversation
    /// context (without it the new message is a detached root).
    #[serde(default)]
    pub last_message_id: Option<String>,
}

/// Per-conversation store of Qwen chat sessions keyed by gateway session id.
///
/// Cleanup: an explicit `DELETE /api/v2/chats/<id>` is expected after the
/// response stream ends. If the consumer disconnects, the session eventually
/// expires via TTL eviction rather than active deletion.
#[derive(Clone)]
pub struct QwenSessionStore {
    inner: Arc<Mutex<HashMap<String, QwenSessionState>>>,
    session_file: Arc<std::sync::Mutex<Option<PathBuf>>>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl QwenSessionStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            session_file: Arc::new(std::sync::Mutex::new(None)),
            loaded: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create a store with optional disk persistence.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        let store = Self::new();
        if let Some(dir) = data_dir {
            let path = dir.join("qwen.json");
            *store.session_file.lock().unwrap() = Some(path);
        }
        store
    }

    async fn ensure_loaded(&self) {
        if self.loaded.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let path = self.session_file.lock().unwrap().clone();
        if let Some(path) = path {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                if let Ok(persisted) = serde_json::from_str::<HashMap<String, QwenSessionState>>(&content) {
                    let mut store = self.inner.lock().await;
                    *store = persisted;
                }
            }
        }
        self.loaded.store(true, std::sync::atomic::Ordering::Release);
    }

    async fn save_to_disk(&self) {
        let path = self.session_file.lock().unwrap().clone();
        let Some(path) = path else { return };
        let store = self.inner.lock().await;
        let json = serde_json::to_string(&*store).unwrap_or_default();
        drop(store);
        let tmp = path.with_extension("tmp");
        if tokio::fs::write(&tmp, &json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &path).await;
        }
    }

    /// Store the association between a gateway session id and the upstream
    /// Qwen chat session. Returns the previous value if one existed.
    pub async fn insert(
        &self,
        gateway_session_id: String,
        state: QwenSessionState,
    ) -> Option<QwenSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.insert(gateway_session_id, state);
        drop(store);
        self.save_to_disk().await;
        prev
    }

    /// Look up an upstream chat_id by gateway session id.
    pub async fn get(&self, gateway_session_id: &str) -> Option<QwenSessionState> {
        self.ensure_loaded().await;
        let store = self.inner.lock().await;
        store.get(gateway_session_id).cloned()
    }

    /// Remove and return the session state. Caller should use `chat_id` to
    /// issue a DELETE to the upstream.
    pub async fn remove(&self, gateway_session_id: &str) -> Option<QwenSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.remove(gateway_session_id);
        drop(store);
        self.save_to_disk().await;
        prev
    }

    pub async fn store_tool_calls(&self, gateway_session_id: &str, calls: &[crate::models::ToolCall]) {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        if let Some(state) = store.get_mut(gateway_session_id) {
            for call in calls {
                state.tool_calls.insert(call.id.clone(), call.clone());
            }
        }
        drop(store);
        self.save_to_disk().await;
    }

    pub async fn get_tool_call(&self, gateway_session_id: &str, call_id: &str) -> Option<crate::models::ToolCall> {
        self.ensure_loaded().await;
        let store = self.inner.lock().await;
        store.get(gateway_session_id)?.tool_calls.get(call_id).cloned()
    }

    /// Record the id of the last assistant message for the given session.
    pub async fn store_last_message_id(&self, gateway_session_id: &str, message_id: &str) {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        if let Some(state) = store.get_mut(gateway_session_id) {
            state.last_message_id = Some(message_id.to_string());
        }
        drop(store);
        self.save_to_disk().await;
    }
}

impl Default for QwenSessionStore {
    fn default() -> Self {
        Self::new()
    }
}
