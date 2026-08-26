use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Persisted anti-bot quarantine so a gateway restart does not immediately
/// re-hammer an upstream that just served a 403 challenge.
///
/// Cooldown mirrors the circuit breaker's `challenge_required` class
/// (20 min); escalation across restarts uses the consecutive counter.
pub const ANTIBOT_QUARANTINE_SECS: u64 = 20 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AntiBotState {
    /// Unix seconds at which the quarantine expires.
    quarantined_until_unix: u64,
    #[serde(default)]
    consecutive_403s: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRelease {
    pub value: String,
    /// Unix seconds when the release was scraped.
    pub scraped_at_unix: u64,
}

/// Re-scrape the release at most this often (grok.com deploys are infrequent).
pub const RELEASE_TTL_SECS: u64 = 6 * 60 * 60;

/// On-disk envelope: conversations plus the anti-bot section. Older files
/// were a bare conversations map and are still readable (`deny_unknown_fields`
/// makes the envelope parse fail on bare maps so the legacy path runs).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedFile {
    #[serde(default)]
    conversations: HashMap<String, ConversationState>,
    #[serde(default)]
    anti_bot: Option<AntiBotState>,
    /// Cached grok.com deploy release (sentry-release build hash) with the
    /// unix time it was scraped. The upstream rejects requests whose declared
    /// release does not match the live deploy ("page out of date").
    release: Option<CachedRelease>,
    /// Harvested signed x-statsig-id with scrape time.
    statsig: Option<CachedRelease>,
    #[serde(default)]
    token_pool: Vec<String>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[allow(dead_code)]
#[derive(Clone, Default)]
pub struct GrokSessionStore {
    inner: std::sync::Arc<Mutex<HashMap<String, ConversationState>>>,
    session_file: std::sync::Arc<Mutex<Option<PathBuf>>>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
    anti_bot: std::sync::Arc<Mutex<Option<AntiBotState>>>,
    release: std::sync::Arc<Mutex<Option<CachedRelease>>>,
    statsig: std::sync::Arc<Mutex<Option<CachedRelease>>>,
    token_pool: std::sync::Arc<Mutex<Vec<String>>>,
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
                // New envelope format first; fall back to the legacy bare
                // conversations map written before anti-bot persistence.
                match serde_json::from_str::<PersistedFile>(&content) {
                    Ok(persisted) => {
                        *self.inner.lock().unwrap() = persisted.conversations;
                        *self.anti_bot.lock().unwrap() = persisted.anti_bot;
                        *self.release.lock().unwrap() = persisted.release;
                        *self.statsig.lock().unwrap() = persisted.statsig;
                        *self.token_pool.lock().unwrap() = persisted.token_pool;
                    }
                    Err(_) => {
                        if let Ok(persisted) =
                            serde_json::from_str::<HashMap<String, ConversationState>>(&content)
                        {
                            *self.inner.lock().unwrap() = persisted;
                        }
                    }
                }
            }
        }
        self.loaded.store(true, std::sync::atomic::Ordering::Release);
    }

    fn save_to_disk(&self) {
        let path = self.session_file.lock().unwrap().clone();
        let Some(path) = path else { return };
        let file = PersistedFile {
            conversations: self.inner.lock().unwrap().clone(),
            anti_bot: self.anti_bot.lock().unwrap().clone(),
            release: self.release.lock().unwrap().clone(),
            statsig: self.statsig.lock().unwrap().clone(),
            token_pool: self.token_pool.lock().unwrap().clone(),
        };
        let json = serde_json::to_string(&file).unwrap_or_default();
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Record an exhausted 403 anti-bot exchange and arm the persisted
    /// quarantine. Returns the remaining quarantine seconds.
    pub fn record_anti_bot_403(&self) -> u64 {
        self.ensure_loaded();
        let mut guard = self.anti_bot.lock().unwrap();
        let consecutive = guard.as_ref().map(|a| a.consecutive_403s).unwrap_or(0) + 1;
        let escalate = ANTIBOT_QUARANTINE_SECS.saturating_mul(consecutive.min(3) as u64);
        let until = unix_now() + escalate;
        *guard = Some(AntiBotState {
            quarantined_until_unix: until,
            consecutive_403s: consecutive,
        });
        drop(guard);
        self.save_to_disk();
        escalate
    }

    /// Remaining quarantine time, if one is armed.
    pub fn anti_bot_quarantine_remaining(&self) -> Option<Duration> {
        self.ensure_loaded();
        let guard = self.anti_bot.lock().unwrap();
        let until = guard.as_ref()?.quarantined_until_unix;
        let now = unix_now();
        (until > now).then(|| Duration::from_secs(until - now))
    }

    /// Cached deploy release, if fresh enough to trust.
    pub fn cached_release(&self) -> Option<String> {
        self.ensure_loaded();
        let guard = self.release.lock().unwrap();
        let cached = guard.as_ref()?;
        let age = unix_now().saturating_sub(cached.scraped_at_unix);
        (age <= RELEASE_TTL_SECS).then(|| cached.value.clone())
    }

    /// Persist a freshly scraped deploy release.
    pub fn store_release(&self, value: String) {
        self.ensure_loaded();
        *self.release.lock().unwrap() = Some(CachedRelease {
            value,
            scraped_at_unix: unix_now(),
        });
        drop(self.release.lock().unwrap());
        self.save_to_disk();
    }

    /// Drop the cached release (upstream declared it stale).
    pub fn invalidate_release(&self) {
        self.ensure_loaded();
        *self.release.lock().unwrap() = None;
        drop(self.release.lock().unwrap());
        self.save_to_disk();
    }

    /// Cached harvested statsig id, if fresh enough. Statsig tokens are
    /// minted per page session; a conservative 5-minute TTL avoids replaying
    /// a token the upstream may have expired.
    pub fn cached_statsig(&self) -> Option<String> {
        self.ensure_loaded();
        let guard = self.statsig.lock().unwrap();
        let cached = guard.as_ref()?;
        let age = unix_now().saturating_sub(cached.scraped_at_unix);
        (age <= super::statsig::HARVEST_TTL_SECS).then(|| cached.value.clone())
    }

    /// Load captured tokens from the Whelmer harvest file if the pool is
    /// empty. Reads from `OBSCURA_GROK_TOKENS` env or `<data_dir>/grok_tokens.json`.
    pub fn load_token_pool_if_empty(&self) {
        {
            let guard = self.token_pool.lock().unwrap();
            if !guard.is_empty() {
                return;
            }
        }
        let token_file = std::env::var("OBSCURA_GROK_TOKENS").unwrap_or_default();
        let path = if token_file.is_empty() {
            self.session_file
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|f| f.parent().map(|p| p.join("grok_tokens.json")))
        } else {
            Some(std::path::PathBuf::from(token_file))
        };
        let Some(path) = path else { return };
        let Ok(content) = std::fs::read_to_string(&path) else { return };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else { return };
        let Some(tokens) = parsed.get("tokens").and_then(|t| t.as_array()) else { return };
        let pool: Vec<String> = tokens
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        if !pool.is_empty() {
            tracing::info!(count = pool.len(), "loaded captured grok statsig tokens");
            *self.token_pool.lock().unwrap() = pool;
        }
    }

    /// Load captured statsig tokens from a provided list.
    #[allow(dead_code)]
    pub fn load_token_pool(&self, tokens: Vec<String>) {
        self.ensure_loaded();
        let mut guard = self.token_pool.lock().unwrap();
        *guard = tokens;
        drop(guard);
    }

    /// Pop the next captured token from the rotation pool. Returns None when
    /// the pool is empty.
    pub fn next_captured_token(&self) -> Option<String> {
        self.ensure_loaded();
        let mut guard = self.token_pool.lock().unwrap();
        if guard.is_empty() {
            return None;
        }
        Some(guard.remove(0))
    }

    /// Persist a freshly harvested statsig id.
    pub fn store_statsig(&self, value: String) {
        self.ensure_loaded();
        *self.statsig.lock().unwrap() = Some(CachedRelease {
            value,
            scraped_at_unix: unix_now(),
        });
        drop(self.statsig.lock().unwrap());
        self.save_to_disk();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("grok-state-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn anti_bot_quarantine_survives_restart() {
        let dir = temp_dir();

        // First "process": exhaust the 403 exchange, quarantine is persisted.
        let quarantine_secs = {
            let store = GrokSessionStore::with_data_dir(Some(dir.clone()));
            let secs = store.record_anti_bot_403();
            assert_eq!(secs, ANTIBOT_QUARANTINE_SECS);
            secs
        }; // Drop persists.

        // Second "process": a fresh store must see the armed quarantine.
        let reopened = GrokSessionStore::with_data_dir(Some(dir.clone()));
        let remaining = reopened.anti_bot_quarantine_remaining().expect("quarantined");
        assert!(
            remaining <= Duration::from_secs(quarantine_secs)
                && remaining > Duration::from_secs(quarantine_secs - 5),
            "remaining should be ~quarantine budget, got {remaining:?}"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn legacy_bare_map_file_still_loads() {
        let dir = temp_dir();
        let path = dir.join("grok.json");
        std::fs::write(
            &path,
            r#"{"conv1":{"conversation_id":"conv1","parent_response_id":null,"model_name":"grok-4","tool_calls":{}}}"#,
        )
        .unwrap();

        let store = GrokSessionStore::with_data_dir(Some(dir.clone()));
        assert!(store.anti_bot_quarantine_remaining().is_none());
        assert!(store.get("conv1").is_some(), "legacy conversations survive");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn quarantine_expires() {
        let dir = temp_dir();
        let path = dir.join("grok.json");
        {
            let store = GrokSessionStore::with_data_dir(Some(dir.clone()));
            // Simulate a long-expired quarantine by writing the envelope directly.
            let file = PersistedFile {
                conversations: HashMap::new(),
                anti_bot: Some(AntiBotState {
                    quarantined_until_unix: unix_now().saturating_sub(10),
                    consecutive_403s: 2,
                }),
                release: None,
                statsig: None,
                token_pool: Vec::new(),
            };
            std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
            drop(store);
        }
        let reopened = GrokSessionStore::with_data_dir(Some(dir.clone()));
        assert!(reopened.anti_bot_quarantine_remaining().is_none(), "expired quarantine must not block");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn release_cache_roundtrip_and_invalidation() {        let dir = temp_dir();
        {
            let store = GrokSessionStore::with_data_dir(Some(dir.clone()));
            assert!(store.cached_release().is_none());
            store.store_release("40480cda06c2c96e3460c999ebbad66c144d2b47f7".to_string());
            assert_eq!(
                store.cached_release().as_deref(),
                Some("40480cda06c2c96e3460c999ebbad66c144d2b47f7")
            );
        }
        // Persists across restart.
        let reopened = GrokSessionStore::with_data_dir(Some(dir.clone()));
        assert_eq!(
            reopened.cached_release().as_deref(),
            Some("40480cda06c2c96e3460c999ebbad66c144d2b47f7")
        );
        reopened.invalidate_release();
        assert!(reopened.cached_release().is_none());

        std::fs::remove_dir_all(dir).ok();
    }
}
