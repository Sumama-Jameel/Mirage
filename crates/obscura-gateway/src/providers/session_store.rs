use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::error::GatewayError;
use crate::models::ToolCall;

/// Default time-to-live for a tracked session.
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// Default cap on tracked sessions to prevent unbounded memory growth.
const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// Serialisable representation of a session for disk persistence.
#[derive(Serialize, Deserialize)]
struct PersistedSession {
    last_used_unix_ms: u128,
    tool_calls: HashMap<String, ToolCall>,
    data: HashMap<String, String>,
}

/// A single tracked session.
struct TrackedSession {
    last_used: Instant,
    tool_calls: HashMap<String, ToolCall>,
    /// Provider-specific key-value data (conversation IDs, model types, etc.)
    data: HashMap<String, String>,
}

/// Generic in-memory session store with optional disk persistence.
///
/// Tracks sessions by an opaque key (e.g. a session token). Each entry
/// carries provider-specific `data` and tool-call storage.
///
/// When `session_file` is set, the store auto-saves to disk after every
/// mutation (`insert`, `set_data`, `store_tool_calls`, `remove`).
/// Use `load_from_file` or `load_or_new` to restore a persisted store.
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<Mutex<HashMap<String, TrackedSession>>>,
    ttl: Duration,
    max_entries: usize,
    session_file: Option<Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>,
    loaded: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionStore {
    /// Create a store with default TTL and size limits.
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_TTL, DEFAULT_MAX_ENTRIES)
    }

    /// Create a store with explicit limits.
    pub fn with_limits(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            ttl,
            max_entries,
            session_file: None,
            loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Enable disk persistence for this store, writing to `path` after mutations.
    pub fn persist_to(&mut self, path: std::path::PathBuf) {
        self.session_file = Some(Arc::new(std::sync::Mutex::new(Some(path))));
    }

    /// Load sessions from a JSON file and return a store backed by it.
    ///
    /// If the file doesn't exist or is corrupt, returns an empty store.
    /// The file path is remembered for future auto-saves.
    #[allow(dead_code)]
    pub async fn load_from_file(path: &Path) -> Self {
        let mut store = Self::new();
        store.persist_to(path.to_path_buf());
        store.reload_from_disk().await;
        store
    }

    /// Create a store backed by a file path, loading existing data if the
    /// file exists. The path is remembered for future auto-saves.
    pub fn with_data_dir(data_dir: Option<std::path::PathBuf>, namespace: &str) -> Self {
        let mut store = Self::new();
        if let Some(dir) = data_dir {
            let path = dir.join(format!("{namespace}.json"));
            store.persist_to(path);
        }
        store
    }

    /// Run once at provider startup: load sessions from the persisted file.
    /// Safe to call even without persistence enabled (no-op).
    #[allow(dead_code)]
    pub async fn load_persisted_sessions(&self) {
        self.reload_from_disk().await;
    }

    async fn reload_from_disk(&self) {
        let path = match &self.session_file {
            Some(p) => p.lock().unwrap().clone(),
            None => return,
        };
        let Some(path) = path else { return };

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(_) => return,
        };

        let persisted: HashMap<String, PersistedSession> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "corrupt session file, starting fresh");
                return;
            }
        };

        let now = Instant::now();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        let mut guard = self.inner.lock().await;
        for (sid, ps) in persisted {
            let age_ms = now_unix.saturating_sub(ps.last_used_unix_ms);
            if age_ms > self.ttl.as_millis() {
                continue; // expired, skip
            }
            guard.insert(sid, TrackedSession {
                last_used: now - Duration::from_millis(age_ms as u64),
                tool_calls: ps.tool_calls,
                data: ps.data,
            });
        }
        tracing::info!(count = guard.len(), file = %path.display(), "sessions loaded from disk");
    }

    async fn save_to_disk(&self) {
        let path = match &self.session_file {
            Some(p) => p.lock().unwrap().clone(),
            None => return,
        };
        let Some(path) = path else { return };

        let guard = self.inner.lock().await;

        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        let mut persisted: HashMap<String, PersistedSession> = HashMap::with_capacity(guard.len());
        for (sid, ts) in guard.iter() {
            persisted.insert(sid.clone(), PersistedSession {
                last_used_unix_ms: now_unix.saturating_sub(
                    ts.last_used.elapsed().as_millis(),
                ),
                tool_calls: ts.tool_calls.clone(),
                data: ts.data.clone(),
            });
        }
        drop(guard); // release lock before file I/O

        let json = serde_json::to_string(&persisted).unwrap_or_default();

        // Atomic write: write to tmp, then rename
        let tmp = path.with_extension("tmp");
        match tokio::fs::write(&tmp, &json).await {
            Ok(_) => {
                if let Err(e) = tokio::fs::rename(&tmp, &path).await {
                    tracing::warn!(file = %path.display(), error = %e, "failed to rename session file");
                }
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "failed to write session file");
            }
        }
    }

    /// Acquire a session: mark it as in-use and refresh its last-used time.
    ///
    /// Returns `None` if the session is unknown or expired.
    /// On first call, loads persisted sessions from disk if configured.
    pub async fn acquire(&self, session_id: &str) -> Option<()> {
        self.ensure_loaded().await;
        let mut guard = self.inner.lock().await;
        self.evict_locked(&mut guard);

        let tracked = guard.get_mut(session_id)?;
        if self.is_expired(tracked) {
            guard.remove(session_id);
            return None;
        }

        tracked.last_used = Instant::now();
        Some(())
    }

    /// Touch a session to update its last-used timestamp.
    #[allow(dead_code)]
    pub async fn touch(&self, session_id: &str) {
        let mut guard = self.inner.lock().await;
        if let Some(tracked) = guard.get_mut(session_id) {
            tracked.last_used = Instant::now();
        }
    }

    /// Insert or update a tracked session with provider data.
    pub async fn insert(&self, session_id: String) {
        self.ensure_loaded().await;
        let mut guard = self.inner.lock().await;
        self.evict_locked(&mut guard);

        guard
            .entry(session_id)
            .or_insert_with(|| TrackedSession {
                last_used: Instant::now(),
                tool_calls: HashMap::new(),
                data: HashMap::new(),
            })
            .last_used = Instant::now();
        drop(guard);
        self.save_to_disk().await;
    }

    /// Remove a session explicitly.
    #[allow(dead_code)]
    pub async fn remove(&self, session_id: &str) {
        self.ensure_loaded().await;
        let mut guard = self.inner.lock().await;
        guard.remove(session_id);
        drop(guard);
        self.save_to_disk().await;
    }

    /// Read a data value stored on a session.
    pub async fn get_data(&self, session_id: &str, key: &str) -> Option<String> {
        self.ensure_loaded().await;
        let mut guard = self.inner.lock().await;
        self.evict_locked(&mut guard);
        guard
            .get_mut(session_id)
            .map(|t| {
                t.last_used = Instant::now();
                t.data.get(key).cloned()
            })
            .flatten()
    }

    /// Write a data value on a session.
    pub async fn set_data(&self, session_id: &str, key: &str, value: String) {
        self.ensure_loaded().await;
        let mut guard = self.inner.lock().await;
        if let Some(tracked) = guard.get_mut(session_id) {
            tracked.last_used = Instant::now();
            tracked.data.insert(key.to_string(), value);
        }
        drop(guard);
        self.save_to_disk().await;
    }

    /// Remove a data key from a session.
    #[allow(dead_code)]
    pub async fn remove_data(&self, session_id: &str, key: &str) {
        self.ensure_loaded().await;
        let mut guard = self.inner.lock().await;
        if let Some(tracked) = guard.get_mut(session_id) {
            tracked.data.remove(key);
        }
        drop(guard);
        self.save_to_disk().await;
    }

    /// Ensure sessions are loaded from disk on first access.
    async fn ensure_loaded(&self) {
        if self.loaded.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.reload_from_disk().await;
        self.loaded.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Store tool calls emitted by the assistant in a session.
    pub async fn store_tool_calls(&self, session_id: &str, calls: &[ToolCall]) {
        let mut guard = self.inner.lock().await;
        self.evict_locked(&mut guard);

        if let Some(tracked) = guard.get_mut(session_id) {
            for call in calls {
                tracked.tool_calls.insert(call.id.clone(), call.clone());
            }
            tracked.last_used = Instant::now();
        }
        drop(guard);
        self.save_to_disk().await;
    }

    /// Retrieve a previously stored tool call by id.
    pub async fn get_tool_call(&self, session_id: &str, call_id: &str) -> Option<ToolCall> {
        self.ensure_loaded().await;
        let mut guard = self.inner.lock().await;
        self.evict_locked(&mut guard);

        guard.get_mut(session_id).and_then(|tracked| {
            tracked.last_used = Instant::now();
            tracked.tool_calls.get(call_id).cloned()
        })
    }

    /// Check if any session currently holds the given data key=value pair.
    /// Returns the session id if found.
    #[allow(dead_code)]
    pub async fn find_by_data(&self, key: &str, value: &str) -> Option<String> {
        let guard = self.inner.lock().await;
        for (sid, tracked) in guard.iter() {
            if tracked.data.get(key).map(|v| v.as_str()) == Some(value) {
                return Some(sid.clone());
            }
        }
        None
    }

    fn is_expired(&self, tracked: &TrackedSession) -> bool {
        tracked.last_used.elapsed() > self.ttl
    }

    fn evict_locked(&self, guard: &mut HashMap<String, TrackedSession>) {
        let now = Instant::now();
        guard.retain(|_, tracked| now.duration_since(tracked.last_used) <= self.ttl);

        if guard.len() > self.max_entries {
            let mut items: Vec<_> = guard.iter().collect();
            items.sort_by_key(|(_, tracked)| tracked.last_used);
            let to_remove = items.len() - self.max_entries;
            let keys: Vec<String> = items
                .into_iter()
                .take(to_remove)
                .map(|(k, _)| k.clone())
                .collect();
            for key in keys {
                guard.remove(&key);
            }
        }
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate that a requested model matches a tracked session's model.
pub fn ensure_model_matches(
    requested_model: &str,
    tracked_model: &str,
) -> Result<(), GatewayError> {
    if requested_model != tracked_model {
        return Err(GatewayError::BadRequest(format!(
            "session was created with model '{}', but request asks for '{}'",
            tracked_model, requested_model
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{FunctionCall, ToolCall};

    #[tokio::test]
    async fn store_tracks_and_retrieves_session() {
        let store = SessionStore::new();
        store.insert("s1".to_string()).await;
        store.set_data("s1", "model", "default".to_string()).await;

        assert!(store.acquire("s1").await.is_some());
        assert_eq!(
            store.get_data("s1", "model").await,
            Some("default".to_string())
        );
    }

    #[tokio::test]
    async fn expired_session_is_removed() {
        let store = SessionStore::with_limits(Duration::from_millis(1), 100);
        store.insert("s1".to_string()).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(store.acquire("s1").await.is_none());
    }

    #[tokio::test]
    async fn cap_evicts_oldest() {
        let store = SessionStore::with_limits(Duration::from_secs(60), 2);
        store.insert("s1".to_string()).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        store.insert("s2".to_string()).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        store.insert("s3".to_string()).await;

        assert!(store.acquire("s1").await.is_none());
        assert!(store.acquire("s2").await.is_some());
        assert!(store.acquire("s3").await.is_some());
    }

    #[tokio::test]
    async fn store_and_retrieve_tool_call() {
        let store = SessionStore::new();
        store.insert("s1".to_string()).await;
        store.set_data("s1", "model", "default".to_string()).await;

        let call = ToolCall {
            id: "call_1".to_string(),
            r#type: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: r#"{"path":"main.py"}"#.to_string(),
            },
        };
        store.store_tool_calls("s1", &[call.clone()]).await;

        let retrieved = store.get_tool_call("s1", "call_1").await.unwrap();
        assert_eq!(retrieved.id, "call_1");
        assert_eq!(retrieved.function.name, "read_file");
        assert_eq!(retrieved.function.arguments, r#"{"path":"main.py"}"#);

        assert!(store.get_tool_call("s1", "missing").await.is_none());
        assert!(store.get_tool_call("s2", "call_1").await.is_none());
    }

    #[tokio::test]
    async fn find_by_data_works() {
        let store = SessionStore::new();
        store.insert("s1".to_string()).await;
        store.set_data("s1", "conversation_id", "abc".to_string()).await;
        store.set_data("s1", "response_id", "def".to_string()).await;

        store.insert("s2".to_string()).await;
        store.set_data("s2", "conversation_id", "xyz".to_string()).await;

        assert_eq!(
            store.find_by_data("conversation_id", "abc").await,
            Some("s1".to_string())
        );
        assert_eq!(
            store.find_by_data("conversation_id", "xyz").await,
            Some("s2".to_string())
        );
        assert!(store.find_by_data("conversation_id", "missing").await.is_none());
    }

    #[tokio::test]
    async fn remove_data_works() {
        let store = SessionStore::new();
        store.insert("s1".to_string()).await;
        store.set_data("s1", "key1", "val1".to_string()).await;
        assert_eq!(store.get_data("s1", "key1").await, Some("val1".to_string()));

        store.remove_data("s1", "key1").await;
        assert_eq!(store.get_data("s1", "key1").await, None);
    }
}
