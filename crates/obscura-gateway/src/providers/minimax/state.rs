use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinimaxSessionState {
    pub session_id: String,
    pub agent_name: String,
    pub device_id: String,
    pub user_id: String,
    pub uuid: String,
    pub op_ticket: String,
    pub model: String,
    pub turn_counter: u64,
    #[serde(default)]
    pub tool_calls: HashMap<String, crate::models::ToolCall>,
}

#[derive(Clone)]
pub struct MinimaxSessionStore {
    inner: Arc<Mutex<HashMap<String, MinimaxSessionState>>>,
    session_file: Arc<std::sync::Mutex<Option<PathBuf>>>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MinimaxSessionStore {
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
            let path = dir.join("minimax.json");
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
                if let Ok(persisted) = serde_json::from_str::<HashMap<String, MinimaxSessionState>>(&content) {
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
        state: MinimaxSessionState,
    ) -> Option<MinimaxSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.insert(gateway_session_id, state);
        drop(store);
        self.save_to_disk().await;
        prev
    }

    pub async fn get(&self, gateway_session_id: &str) -> Option<MinimaxSessionState> {
        self.ensure_loaded().await;
        let store = self.inner.lock().await;
        store.get(gateway_session_id).cloned()
    }

    pub async fn remove(&self, gateway_session_id: &str) -> Option<MinimaxSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        let prev = store.remove(gateway_session_id);
        drop(store);
        self.save_to_disk().await;
        prev
    }

    pub async fn next_turn(&self, gateway_session_id: &str) -> Option<MinimaxSessionState> {
        self.ensure_loaded().await;
        let mut store = self.inner.lock().await;
        if let Some(state) = store.get_mut(gateway_session_id) {
            state.turn_counter += 1;
            let result = Some(state.clone());
            drop(store);
            self.save_to_disk().await;
            result
        } else {
            None
        }
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
}

impl Default for MinimaxSessionStore {
    fn default() -> Self {
        Self::new()
    }
}
