use std::path::PathBuf;

use crate::error::GatewayError;
use crate::models::ToolCall;
use crate::providers::session_store::SessionStore as SharedStore;

const KEY_CHAT_ID: &str = "chat_id";
const KEY_MODEL_ID: &str = "model_id";
const KEY_SEGMENT_ID: &str = "segment_id";

#[derive(Debug, Clone)]
pub struct StoredConversation {
    pub chat_id: String,
    pub model_id: String,
    pub segment_id: Option<String>,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: SharedStore,
    pub upload_cache: crate::providers::kimi::upload::UploadCache,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: SharedStore::new(),
            upload_cache: crate::providers::kimi::upload::UploadCache::new(),
        }
    }

    /// Create a store with optional disk persistence.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            inner: SharedStore::with_data_dir(data_dir, "kimi"),
            upload_cache: crate::providers::kimi::upload::UploadCache::new(),
        }
    }

    pub async fn acquire(&self, session_id: &str) -> Option<StoredConversation> {
        self.inner.acquire(session_id).await?;
        let chat_id = self.inner.get_data(session_id, KEY_CHAT_ID).await?;
        let model_id = self
            .inner
            .get_data(session_id, KEY_MODEL_ID)
            .await
            .unwrap_or_default();
        let segment_id = self.inner.get_data(session_id, KEY_SEGMENT_ID).await;
        Some(StoredConversation {
            chat_id,
            model_id,
            segment_id,
        })
    }

    pub async fn insert(
        &self,
        session_id: String,
        conv: &StoredConversation,
        model_id: &str,
    ) {
        self.inner.insert(session_id.clone()).await;
        self.inner
            .set_data(&session_id, KEY_CHAT_ID, conv.chat_id.clone())
            .await;
        self.inner
            .set_data(&session_id, KEY_MODEL_ID, model_id.to_string())
            .await;
        if let Some(ref segment_id) = conv.segment_id {
            self.inner
                .set_data(&session_id, KEY_SEGMENT_ID, segment_id.clone())
                .await;
        }
    }

    #[allow(dead_code)]
    pub async fn update_segment_id(&self, session_id: &str, segment_id: &str) {
        self.inner
            .set_data(session_id, KEY_SEGMENT_ID, segment_id.to_string())
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
