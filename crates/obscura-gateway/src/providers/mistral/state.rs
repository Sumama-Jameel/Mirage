use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Persistent Mistral Le Chat conversation state keyed by gateway session id.
///
/// Mistral continues a conversation by appending to the same chat: the create
/// request (no `chatId`) starts a new thread and the stream's `bootstrap`
/// state update carries `chat.id`; later turns send `mode: "append"` with that
/// `chatId` and a fresh client-generated `messageId`. `message_version` is the
/// last assistant message version observed in the stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MistralSessionState {
    pub chat_id: String,
    pub message_id: String,
    pub message_version: i64,
    pub model: String,
    /// Feature ids (e.g. `beta-reasoning`) sent on the last turn so the
    /// continuation preserves the same capabilities.
    pub features: Vec<String>,
    /// Stable anonymous identifier persisted per chat, mirroring the web
    /// app's per-browser `stableAnonymousIdentifier`.
    pub anonymous_identifier: String,
}

impl MistralSessionState {
    /// Used by tests and by callers constructing continuation state.
    #[allow(dead_code)]
    pub fn new(chat_id: String, model: String, features: Vec<String>) -> Self {
        Self {
            chat_id,
            message_id: String::new(),
            message_version: 0,
            model,
            features,
            anonymous_identifier: String::new(),
        }
    }
}

/// Disk-persisted store of Mistral conversation state, mirroring the minimax
/// `SessionStore` pattern.
#[derive(Clone)]
pub struct MistralSessionStore {
    inner: Arc<Mutex<HashMap<String, MistralSessionState>>>,
    session_file: Arc<std::sync::Mutex<Option<PathBuf>>>,
    /// Serializes disk writes so two concurrent saves cannot interleave on the
    /// same tmp file.
    save_lock: Arc<tokio::sync::Mutex<()>>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MistralSessionStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            session_file: Arc::new(std::sync::Mutex::new(None)),
            save_lock: Arc::new(tokio::sync::Mutex::new(())),
            loaded: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create a store with optional disk persistence.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        let store = Self::new();
        if let Some(dir) = data_dir {
            let path = dir.join("mistral.json");
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
                if let Ok(persisted) =
                    serde_json::from_str::<HashMap<String, MistralSessionState>>(&content)
                {
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
        // Serialize concurrent saves on the same file.
        let _guard = self.save_lock.lock().await;
        let store = self.inner.lock().await;
        let json = serde_json::to_string(&*store).unwrap_or_default();
        drop(store);
        let tmp = path.with_extension("tmp");
        if tokio::fs::write(&tmp, &json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &path).await;
        }
    }

    /// Insert or replace the state for a session. Used by tests; production
    /// writes go through `update`.
    #[allow(dead_code)]
    pub async fn insert(
        &self,
        gateway_session_id: String,
        state: MistralSessionState,
    ) -> Option<MistralSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.insert(gateway_session_id, state);
        drop(store);
        self.save_to_disk().await;
        prev
    }

    pub async fn get(&self, gateway_session_id: &str) -> Option<MistralSessionState> {
        self.ensure_loaded().await;
        let store = self.inner.lock().await;
        store.get(gateway_session_id).cloned()
    }

    /// Remove the state for a session. Used by tests; production sessions are
    /// kept for continuation.
    #[allow(dead_code)]
    pub async fn remove(&self, gateway_session_id: &str) -> Option<MistralSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.remove(gateway_session_id);
        drop(store);
        self.save_to_disk().await;
        prev
    }

    pub async fn update(
        &self,
        gateway_session_id: &str,
        state: MistralSessionState,
    ) -> Option<MistralSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.insert(gateway_session_id.to_string(), state);
        drop(store);
        self.save_to_disk().await;
        prev
    }
}

impl Default for MistralSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mistral_session_state_round_trips_json() {
        let state = MistralSessionState::new(
            "chat-123".to_string(),
            "mistral-large-latest".to_string(),
            vec!["beta-reasoning".to_string()],
        );
        let json = serde_json::to_string(&state).unwrap();
        let back: MistralSessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.chat_id, "chat-123");
        assert_eq!(back.model, "mistral-large-latest");
        assert_eq!(back.features, vec!["beta-reasoning"]);
    }

    #[test]
    fn mistral_state_defaults_anonymous_identifier() {
        let state = MistralSessionState::new(
            "chat-123".to_string(),
            "mistral-large-latest".to_string(),
            vec![],
        );
        assert_eq!(state.anonymous_identifier, "");
    }

    #[tokio::test]
    async fn mistral_store_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = MistralSessionStore::with_data_dir(Some(dir.path().to_path_buf()));
        store
            .insert(
                "gw-session".to_string(),
                MistralSessionState::new(
                    "chat-abc".to_string(),
                    "mistral-medium-latest".to_string(),
                    vec![],
                ),
            )
            .await;

        let reloaded = MistralSessionStore::with_data_dir(Some(dir.path().to_path_buf()));
        let got = reloaded.get("gw-session").await.unwrap();
        assert_eq!(got.chat_id, "chat-abc");
        assert_eq!(got.model, "mistral-medium-latest");
    }
}
