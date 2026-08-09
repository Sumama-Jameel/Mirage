use std::path::PathBuf;

use crate::error::GatewayError;
use crate::models::ToolCall;
use crate::providers::session_store::SessionStore as SharedStore;

const KEY_ORG_ID: &str = "org_id";
const KEY_MODEL_ID: &str = "model_id";

#[derive(Debug, Clone)]
pub struct StoredConversation {
    pub org_id: String,
    pub model_id: String,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: SharedStore,
    pub upload_cache: crate::providers::claude::upload::UploadCache,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: SharedStore::new(),
            upload_cache: crate::providers::claude::upload::UploadCache::new(),
        }
    }

    /// Create a store with optional disk persistence.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            inner: SharedStore::with_data_dir(data_dir, "claude"),
            upload_cache: crate::providers::claude::upload::UploadCache::new(),
        }
    }

    pub async fn acquire(&self, session_id: &str) -> Option<StoredConversation> {
        self.inner.acquire(session_id).await?;
        let org_id = self.inner.get_data(session_id, KEY_ORG_ID).await?;
        let model_id = self
            .inner
            .get_data(session_id, KEY_MODEL_ID)
            .await
            .unwrap_or_default();
        Some(StoredConversation { org_id, model_id })
    }

    pub async fn insert(
        &self,
        session_id: String,
        conv: &StoredConversation,
        model_id: &str,
    ) {
        self.inner.insert(session_id.clone()).await;
        self.inner
            .set_data(&session_id, KEY_ORG_ID, conv.org_id.clone())
            .await;
        self.inner
            .set_data(&session_id, KEY_MODEL_ID, model_id.to_string())
            .await;
    }

    pub async fn store_tool_calls(&self, session_id: &str, calls: &[ToolCall]) {
        self.inner.store_tool_calls(session_id, calls).await;
    }

    pub async fn get_tool_call(&self, session_id: &str, call_id: &str) -> Option<ToolCall> {
        self.inner.get_tool_call(session_id, call_id).await
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
