use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::GatewayError;

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    pub conversation_id: String,
    pub parent_response_id: Option<String>,
    pub model_name: String,
    #[serde(default)]
    pub tool_calls: HashMap<String, crate::models::ToolCall>,
}

#[allow(dead_code)]
#[derive(Clone, Default)]
pub struct GrokSessionStore {
    inner: std::sync::Arc<Mutex<HashMap<String, ConversationState>>>,
    session_file: std::sync::Arc<Mutex<Option<PathBuf>>>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl GrokSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a store with optional disk persistence.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        let store = Self::default();
        if let Some(dir) = data_dir {
            let path = dir.join("grok.json");
            *store.session_file.lock().unwrap() = Some(path);
        }
        store
    }

    fn ensure_loaded(&self) {
        if self.loaded.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let path = self.session_file.lock().unwrap().clone();
        if let Some(path) = path {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(persisted) = serde_json::from_str::<HashMap<String, ConversationState>>(&content) {
                    let mut store = self.inner.lock().unwrap();
                    *store = persisted;
                }
            }
        }
        self.loaded.store(true, std::sync::atomic::Ordering::Release);
    }

    fn save_to_disk(&self) {
        let path = self.session_file.lock().unwrap().clone();
        let Some(path) = path else { return };
        let store = self.inner.lock().unwrap();
        let json = serde_json::to_string(&*store).unwrap_or_default();
        drop(store);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    #[allow(dead_code)]
    pub fn get_or_create(
        &self,
        session_url: Option<&str>,
        model_name: &str,
        conversation_id: String,
    ) -> ConversationState {
        self.ensure_loaded();
        let mut store = self.inner.lock().unwrap();
        if let Some(url) = session_url {
            if let Some(state) = store.get(url) {
                return state.clone();
            }
        }
        let state = ConversationState {
            conversation_id,
            parent_response_id: None,
            model_name: model_name.to_string(),
            tool_calls: HashMap::new(),
        };
        store.insert(state.conversation_id.clone(), state.clone());
        drop(store);
        self.save_to_disk();
        state
    }

    pub fn store_tool_calls(&self, conversation_id: &str, calls: &[crate::models::ToolCall]) {
        self.ensure_loaded();
        let mut store = self.inner.lock().unwrap();
        if let Some(state) = store.get_mut(conversation_id) {
            for call in calls {
                state.tool_calls.insert(call.id.clone(), call.clone());
            }
        }
        drop(store);
        self.save_to_disk();
    }

    pub fn get_tool_call(&self, conversation_id: &str, call_id: &str) -> Option<crate::models::ToolCall> {
        self.ensure_loaded();
        let store = self.inner.lock().unwrap();
        store.get(conversation_id)?.tool_calls.get(call_id).cloned()
    }

    #[allow(dead_code)]
    pub fn update_parent(
        &self,
        conversation_id: &str,
        parent_response_id: Option<String>,
    ) -> Result<(), GatewayError> {
        self.ensure_loaded();
        let mut store = self.inner.lock().unwrap();
        let state = store
            .get_mut(conversation_id)
            .ok_or_else(|| GatewayError::Internal("conversation not found".to_string()))?;
        state.parent_response_id = parent_response_id;
        drop(store);
        self.save_to_disk();
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get(&self, conversation_id: &str) -> Option<ConversationState> {
        self.ensure_loaded();
        let store = self.inner.lock().unwrap();
        store.get(conversation_id).cloned()
    }
}

// Implement Drop to persist on shutdown — best-effort only.
impl Drop for GrokSessionStore {
    fn drop(&mut self) {
        self.save_to_disk();
    }
}
