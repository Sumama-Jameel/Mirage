//! Meta AI conversation-continuation store.
//!
//! The DGW WebSocket transport opens a meta.ai conversation per
//! `conversationId`. Each turn from the gateway's OpenAI-compatible API is a
//! fresh HTTP request with the full message history, so without persistence
//! every turn would start a brand-new meta.ai conversation with the whole
//! history folded into one prompt.
//!
//! To keep a single growing conversation, the store remembers the
//! `conversationId` (+ `branchPath`) a previous turn created, keyed by the
//! session URL handed back to the client in the completion response. When the
//! client passes that `session_url` back on the next turn, the provider
//! reuses the cached conversation: `isNewConversation=false`, and only the
//! latest user turn is sent. This mirrors the reference implementation's
//! continuation cache (hash of the normalized history prefix) with the
//! gateway's native `session_url` mechanism.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    /// Meta's client-generated conversation id (`c.<base62>`), reused across
    /// turns of the same chat thread.
    pub conversation_id: String,
    /// Conversation branch path. Root conversations use `"0"`.
    pub branch_path: String,
    /// Model that created this conversation.
    pub model_name: String,
}

#[derive(Clone, Default)]
pub struct MetaAiSessionStore {
    inner: std::sync::Arc<Mutex<HashMap<String, ConversationState>>>,
    session_file: std::sync::Arc<Mutex<Option<PathBuf>>>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MetaAiSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a store with optional disk persistence.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        let store = Self::default();
        if let Some(dir) = data_dir {
            let path = dir.join("metaai.json");
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
                if let Ok(persisted) =
                    serde_json::from_str::<HashMap<String, ConversationState>>(&content)
                {
                    let mut store = self.inner.lock().unwrap();
                    *store = persisted;
                }
            }
        }
        self.loaded
            .store(true, std::sync::atomic::Ordering::Release);
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

    /// Resolve conversation state for a request.
    ///
    /// When `session_url` names a conversation this store created before, the
    /// cached state is returned (`is_continuation=true`). Otherwise a fresh
    /// state for `conversation_id` is recorded and returned.
    pub fn get_or_create(
        &self,
        session_url: Option<&str>,
        model_name: &str,
        conversation_id: String,
    ) -> (ConversationState, bool) {
        self.ensure_loaded();
        let mut store = self.inner.lock().unwrap();
        if let Some(url) = session_url {
            if let Some(key) = conversation_key_from_url(url) {
                let full_key = format!("https://www.meta.ai/c/{key}");
                if let Some(state) = store.get(&full_key) {
                    return (state.clone(), true);
                }
            }
        }
        let state = ConversationState {
            conversation_id,
            branch_path: "0".to_string(),
            model_name: model_name.to_string(),
        };
        store.insert(
            format!("https://www.meta.ai/c/{}", state.conversation_id),
            state.clone(),
        );
        drop(store);
        self.save_to_disk();
        (state, false)
    }

    /// Persist any state changes (e.g. a server-returned branch path).
    #[allow(dead_code)]
    pub fn update(
        &self,
        conversation_id: &str,
        branch_path: &str,
    ) -> Result<(), crate::error::GatewayError> {
        self.ensure_loaded();
        let mut store = self.inner.lock().unwrap();
        let key = format!("https://www.meta.ai/c/{conversation_id}");
        let state = store
            .get_mut(&key)
            .ok_or_else(|| crate::error::GatewayError::Internal("conversation not found".to_string()))?;
        state.branch_path = branch_path.to_string();
        drop(store);
        self.save_to_disk();
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get(&self, conversation_id: &str) -> Option<ConversationState> {
        self.ensure_loaded();
        let store = self.inner.lock().unwrap();
        store
            .get(&format!("https://www.meta.ai/c/{conversation_id}"))
            .cloned()
    }
}

/// Extract the conversation id from a meta.ai session URL of the form
/// `https://www.meta.ai/c/<conversationId>`.
fn conversation_key_from_url(url: &str) -> Option<&str> {
    let prefix = "https://www.meta.ai/c/";
    url.strip_prefix(prefix)
        .map(|rest| rest.split(['?', '#']).next().unwrap_or(rest))
}

impl Drop for MetaAiSessionStore {
    fn drop(&mut self) {
        self.save_to_disk();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_or_create_new_conversation() {
        let store = MetaAiSessionStore::new();
        let (state, is_continuation) =
            store.get_or_create(None, "muse-spark", "c.abc123".to_string());
        assert!(!is_continuation);
        assert_eq!(state.conversation_id, "c.abc123");
        assert_eq!(state.branch_path, "0");
        assert_eq!(state.model_name, "muse-spark");
    }

    #[test]
    fn get_or_create_reuses_existing_conversation() {
        let store = MetaAiSessionStore::new();
        let (created, _) = store.get_or_create(None, "muse-spark", "c.abc123".to_string());
        let session_url = format!(
            "https://www.meta.ai/c/{}",
            created.conversation_id
        );
        let (reused, is_continuation) =
            store.get_or_create(Some(&session_url), "muse-spark", "c.other".to_string());
        assert!(is_continuation);
        assert_eq!(reused.conversation_id, "c.abc123");
    }

    #[test]
    fn get_or_create_missing_session_url_starts_new() {
        let store = MetaAiSessionStore::new();
        let (state, is_continuation) = store.get_or_create(
            Some("https://www.meta.ai/c/c.unknown"),
            "muse-spark",
            "c.fresh".to_string(),
        );
        assert!(!is_continuation);
        assert_eq!(state.conversation_id, "c.fresh");
    }

    #[test]
    fn conversation_key_from_url_strips_query() {
        assert_eq!(
            conversation_key_from_url("https://www.meta.ai/c/c.abc123?branch=0"),
            Some("c.abc123")
        );
        assert_eq!(
            conversation_key_from_url("https://www.meta.ai/c/c.abc123"),
            Some("c.abc123")
        );
        assert_eq!(conversation_key_from_url("https://other.example/c/x"), None);
    }
}
