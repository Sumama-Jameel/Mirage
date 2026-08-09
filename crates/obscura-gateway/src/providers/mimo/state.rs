use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Persistent MiMo conversation state keyed by gateway session id.
///
/// MiMo keeps a stable `conversationId` per thread: reusing it on the next
/// turn continues the same conversation server-side without resending the
/// full message history. `msgId` is a fresh random id per turn and is not
/// persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiMoSessionState {
    pub conversation_id: String,
    pub model: String,
    pub enable_thinking: bool,
    pub web_search_status: String,
}

/// Disk-persisted store of MiMo conversation state, mirroring the minimax
/// `SessionStore` pattern.
#[derive(Clone)]
pub struct MiMoSessionStore {
    inner: Arc<Mutex<HashMap<String, MiMoSessionState>>>,
    session_file: Arc<std::sync::Mutex<Option<PathBuf>>>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MiMoSessionStore {
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
            let path = dir.join("mimo.json");
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
                    serde_json::from_str::<HashMap<String, MiMoSessionState>>(&content)
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
        let store = self.inner.lock().await;
        let json = serde_json::to_string(&*store).unwrap_or_default();
        drop(store);
        let tmp = path.with_extension("tmp");
        if tokio::fs::write(&tmp, &json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &path).await;
        }
    }

    pub async fn insert(
        &self,
        gateway_session_id: String,
        state: MiMoSessionState,
    ) -> Option<MiMoSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.insert(gateway_session_id, state);
        drop(store);
        self.save_to_disk().await;
        prev
    }

    pub async fn get(&self, gateway_session_id: &str) -> Option<MiMoSessionState> {
        self.ensure_loaded().await;
        let store = self.inner.lock().await;
        store.get(gateway_session_id).cloned()
    }

    pub async fn remove(&self, gateway_session_id: &str) -> Option<MiMoSessionState> {
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
        state: MiMoSessionState,
    ) -> Option<MiMoSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.insert(gateway_session_id.to_string(), state);
        drop(store);
        self.save_to_disk().await;
        prev
    }
}

impl Default for MiMoSessionStore {
    fn default() -> Self {
        Self::new()
    }
}
