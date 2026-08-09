use std::path::PathBuf;

use crate::error::GatewayError;
use crate::models::ToolCall;
use crate::providers::session_store::SessionStore as SharedStore;

const KEY_CONVERSATION_ID: &str = "conversation_id";
const KEY_MESSAGE_ID: &str = "message_id";
const KEY_MODEL_ID: &str = "model_id";

#[derive(Debug, Clone)]
pub struct StoredConversation {
    pub conversation_id: String,
    pub message_id: String,
    pub model_id: String,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: SharedStore,
    pub upload_cache: crate::providers::chatgpt::upload::UploadCache,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            inner: SharedStore::new(),
            upload_cache: crate::providers::chatgpt::upload::UploadCache::new(),
        }
    }

    /// Create a store with optional disk persistence.
    /// Sessions are auto-saved to `{data_dir}/chatgpt.json` when `data_dir` is set.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            inner: SharedStore::with_data_dir(data_dir, "chatgpt"),
            upload_cache: crate::providers::chatgpt::upload::UploadCache::new(),
        }
    }

    pub async fn acquire(&self, session_id: &str) -> Option<StoredConversation> {
        self.inner.acquire(session_id).await?;
        let conv_id = self.inner.get_data(session_id, KEY_CONVERSATION_ID).await?;
        let msg_id = self.inner.get_data(session_id, KEY_MESSAGE_ID).await?;
        let model_id = self
            .inner
            .get_data(session_id, KEY_MODEL_ID)
            .await
            .unwrap_or_default();
        Some(StoredConversation {
            conversation_id: conv_id,
            message_id: msg_id,
            model_id,
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
            .set_data(&session_id, KEY_CONVERSATION_ID, conv.conversation_id.clone())
            .await;
        self.inner
            .set_data(&session_id, KEY_MESSAGE_ID, conv.message_id.clone())
            .await;
        self.inner
            .set_data(&session_id, KEY_MODEL_ID, model_id.to_string())
            .await;
    }

    #[allow(dead_code)]
    pub async fn update_message_id(&self, session_id: &str, message_id: &str) {
        self.inner
            .set_data(session_id, KEY_MESSAGE_ID, message_id.to_string())
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
