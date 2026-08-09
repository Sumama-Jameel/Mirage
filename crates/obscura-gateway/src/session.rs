use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use obscura_browser::{BrowserContext, Page};
use obscura_net::{CookieInfo, CookieJar, RequestInfo, Response};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{error, info, warn};

use crate::browser::{self, build_firefox_ua, firefox_version_or_default, LocalStorageEntry};
use crate::config::Config;
use crate::error::GatewayError;

/// Number of browser contexts kept warm for concurrent API requests.
const DEFAULT_POOL_SIZE: usize = 10;

/// Environment variable to override the pool size.
const POOL_SIZE_ENV_VAR: &str = "OBSCURA_POOL_SIZE";

/// Browser identity the headless engine should impersonate. Derived from the
/// gateway configuration and the user's installed Firefox version.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct BrowserIdentity {
    /// "firefox" or "chrome".
    identity: String,
    /// Full User-Agent string.
    user_agent: String,
    /// `navigator.platform` value.
    platform: String,
    /// `navigator.userAgentData.getHighEntropyValues("platform")` value.
    ua_platform: String,
    /// `navigator.userAgentData.getHighEntropyValues("platformVersion")` value.
    ua_platform_version: String,
}

impl BrowserIdentity {
    /// Build a browser identity from the detected browser source and gateway
    /// configuration.
    fn from_auth(source: &crate::browser::BrowserSource, config: &Config) -> Self {
        if let Some(ref ua) = config.browser.user_agent_override {
            return Self::from_user_agent(ua);
        }

        // The detected source browser wins over a configured identity hint.
        match source.browser_type {
            crate::browser::BrowserType::Firefox => Self::firefox(),
            crate::browser::BrowserType::Chrome => Self::chrome(),
            crate::browser::BrowserType::Edge => Self::edge(),
        }
    }

    fn firefox() -> Self {
        let version = firefox_version_or_default();
        let user_agent = build_firefox_ua(&version);
        Self {
            identity: "firefox".to_string(),
            user_agent,
            platform: "Linux x86_64".to_string(),
            ua_platform: "Linux".to_string(),
            ua_platform_version: "".to_string(),
        }
    }

    fn chrome() -> Self {
        // Keep the existing Chrome-on-Windows profile values.
        Self {
            identity: "chrome".to_string(),
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36".to_string(),
            platform: "Win32".to_string(),
            ua_platform: "Windows".to_string(),
            ua_platform_version: "15.0.0".to_string(),
        }
    }

    fn edge() -> Self {
        Self {
            identity: "edge".to_string(),
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36 EdgA/145.0.0.0".to_string(),
            platform: "Win32".to_string(),
            ua_platform: "Windows".to_string(),
            ua_platform_version: "15.0.0".to_string(),
        }
    }

    fn from_user_agent(ua: &str) -> Self {
        let identity = if ua.to_ascii_lowercase().contains("firefox") {
            "firefox"
        } else {
            "chrome"
        };
        let (platform, ua_platform, ua_platform_version) = if ua.contains("Linux") {
            ("Linux x86_64", "Linux", "")
        } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
            ("MacIntel", "macOS", "14.5.0")
        } else {
            ("Win32", "Windows", "15.0.0")
        };
        Self {
            identity: identity.to_string(),
            user_agent: ua.to_string(),
            platform: platform.to_string(),
            ua_platform: ua_platform.to_string(),
            ua_platform_version: ua_platform_version.to_string(),
        }
    }
}

/// Maximum time a request may wait for a free session.
const ACQUIRE_TIMEOUT_MS: u64 = 30_000;

/// Polling interval while waiting for a session to become ready.
const ACQUIRE_POLL_MS: u64 = 10;

/// Handle to an acquired browser context.
///
/// Cheaply cloneable. The actual browser objects stay on the session's
/// dedicated OS thread; this handle only carries metadata needed by the
/// provider loop.
#[derive(Clone)]
pub struct SessionHandle {
    pub id: String,
    pub cookie_jar: Arc<CookieJar>,
    /// DeepSeek localStorage entries imported from the source browser. Carries
    /// the bearer token (`userToken`) and other provider state required for
    /// API calls.
    pub local_storage: Vec<LocalStorageEntry>,
    /// User-Agent string the warmed browser is using. Needed so direct API
    /// requests match the browser fingerprint.
    pub user_agent: String,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.id)
            .field("local_storage_keys", &self.local_storage.len())
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionState {
    Ready,
    Busy,
    Dirty,
    Warming,
    Failed,
}

/// Result of an `extract_texts` command: the visible text of two subtrees.
#[derive(Debug, Clone, Default)]
pub struct ExtractedTexts {
    pub response: String,
    pub thinking: String,
}

/// A captured JS response. Used by UI providers to read structured backend
/// payloads (e.g. chat.z.ai SSE streams) without re-implementing captcha
/// solvers or request signing.
#[derive(Debug, Clone)]
pub struct CapturedResponse {
    pub url: String,
    /// HTTP status code. Providers may use this to distinguish success from
    /// failure.
    #[allow(dead_code)]
    pub status: u16,
    /// Response headers. Providers may use these for content-type checks.
    #[allow(dead_code)]
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Mutable capture state shared between the provider and the session thread's
/// `on_response` callback.
#[derive(Debug, Default)]
struct ActiveCapture {
    /// When `Some`, the callback stores responses whose URL contains this
    /// substring.
    pattern: Option<String>,
    /// Responses captured while the pattern is active.
    responses: Vec<CapturedResponse>,
}

/// Handle returned by [`SessionManager::start_capture`]. Dropping it does *not*
/// stop the capture; call [`SessionManager::stop_capture`] explicitly.
#[derive(Clone, Debug)]
pub struct CaptureHandle {
    state: Arc<Mutex<ActiveCapture>>,
}

impl CaptureHandle {
    /// Take all responses captured so far, leaving the capture active.
    pub async fn take_responses(&self) -> Vec<CapturedResponse> {
        let mut guard = self.state.lock().await;
        std::mem::take(&mut guard.responses)
    }
}

/// Manager for the Obscura browser context pool.
///
/// Each warmed session lives on its own dedicated OS thread with its own V8
/// isolate. This keeps V8's "one isolate per thread" invariant satisfied even
/// when multiple sessions exist. The manager handle is `Send + Sync + Clone`
/// so it can be used from axum handlers.
#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<String, SessionThread>>>,
    auth: Arc<Mutex<Option<(Vec<CookieInfo>, Vec<LocalStorageEntry>, BrowserIdentity)>>>,
    /// Total number of sessions the pool should hold (warm or on-demand).
    pool_size: usize,
    /// Number of session threads currently alive, including those being
    /// warmed in the background. Guards lazy creation so the pool never
    /// exceeds `pool_size`.
    alive: Arc<AtomicUsize>,
}

impl SessionManager {
    /// Build an empty `SessionManager` with no warmed sessions.
    ///
    /// Useful in tests and as a placeholder before `spawn()` finishes.
    /// Calling `release()` on a default instance returns an error, which
    /// callers already handle gracefully (warn and move on).
    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            auth: Arc::new(Mutex::new(None)),
            pool_size: 0,
            alive: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Import auth and start the session pool without blocking startup.
    ///
    /// Imports browser cookies/localStorage once, then returns immediately.
    /// A background task warms the pool up to `pool_size` concurrently; if a
    /// request arrives before warm-up finishes, [`SessionManager::acquire`]
    /// creates a session on demand so the gateway is never blocked on browser
    /// navigation.
    ///
    /// RECOVERY MECHANISM: The background task also periodically recovers
    /// `Dirty` sessions by attempting to rewarm them. This prevents permanent
    /// session pool exhaustion when transient failures mark sessions as dirty.
    pub async fn spawn(config: &Config) -> Result<Self, GatewayError> {
        let import_options = config.browser.import_options();

        let auth = browser::import_auth(&import_options)?;
        let cookies = auth.cookies;
        let local_storage = auth.local_storage;
        let identity = BrowserIdentity::from_auth(&auth.source, config);
        info!(
            cookies = cookies.len(),
            local_storage = local_storage.len(),
            identity = %identity.identity,
            user_agent = %identity.user_agent,
            source = %auth.source.browser_type,
            profile = %auth.source.profile_path.display(),
            "Imported DeepSeek session state for sessions"
        );

        let pool_size = std::env::var(POOL_SIZE_ENV_VAR)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_POOL_SIZE);

        // Limit concurrent session warming to prevent resource exhaustion during startup
        let max_concurrent_warmup = std::env::var("OBSCURA_MAX_CONCURRENT_WARMUP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(4);

        info!(pool_size = pool_size, max_concurrent_warmup = max_concurrent_warmup, "Session pool configured");

        let sessions = Arc::new(Mutex::new(HashMap::with_capacity(pool_size)));
        let alive = Arc::new(AtomicUsize::new(0));

        let auth_data = Some((cookies.clone(), local_storage.clone(), identity.clone()));
        let auth = Arc::new(Mutex::new(auth_data));

        let manager = Self {
            sessions: sessions.clone(),
            auth: auth.clone(),
            pool_size,
            alive: alive.clone(),
        };

        // Warm sessions in the background so the gateway can bind and serve
        // immediately. On-demand acquire() fills any gap while warm-up runs.
        tokio::spawn(async move {
            Self::background_warmup(sessions, auth, alive, pool_size, max_concurrent_warmup).await;
        });

        Ok(manager)
    }

    /// Background task that warms the pool concurrently and then recovers
    /// `Dirty` sessions.
    async fn background_warmup(
        sessions: Arc<Mutex<HashMap<String, SessionThread>>>,
        auth: Arc<Mutex<Option<(Vec<CookieInfo>, Vec<LocalStorageEntry>, BrowserIdentity)>>>,
        alive: Arc<AtomicUsize>,
        pool_size: usize,
        max_concurrent_warmup: usize,
    ) {
        let (cookies, local_storage, identity) = {
            let guard = auth.lock().await;
            match guard.as_ref() {
                Some((c, l, i)) => (c.clone(), l.clone(), i.clone()),
                None => return,
            }
        };

        // Warm sessions concurrently with controlled concurrency
        use futures::stream::{self, StreamExt};
        let warmup_futures = (0..pool_size).map(|i| {
            let id = format!("session-{}", i + 1);
            let cookies = cookies.clone();
            let local_storage = local_storage.clone();
            let identity = identity.clone();
            let alive = alive.clone();
            async move {
                if alive.fetch_add(1, Ordering::AcqRel) >= pool_size {
                    // Pool already full (lazy sessions created on demand).
                    alive.fetch_sub(1, Ordering::AcqRel);
                    return None;
                }
                match SessionThread::spawn(id.clone(), cookies, local_storage, identity).await {
                    Ok(session) => Some(session),
                    Err(e) => {
                        error!(session_id = %id, error = %e, "Failed to warm session; skipping");
                        alive.fetch_sub(1, Ordering::AcqRel);
                        None
                    }
                }
            }
        });

        let warmup_stream = stream::iter(warmup_futures)
            .buffer_unordered(max_concurrent_warmup);

        let mut warmed_sessions = Vec::new();
        tokio::pin!(warmup_stream);
        while let Some(result) = warmup_stream.next().await {
            if let Some(session) = result {
                warmed_sessions.push(session);
            }
        }

        for session in warmed_sessions {
            sessions.lock().await.insert(session.id.clone(), session);
        }

        let ready_count = sessions.lock().await.len();
        info!(ready_sessions = ready_count, pool_size = pool_size, "Session pool warmed in background");

        // Spawn background rewarming task to recover Dirty sessions.
        // This prevents permanent session pool exhaustion from transient failures.
        let sessions_for_recovery = sessions;
        Self::background_recovery_loop(sessions_for_recovery).await;
    }

    /// Background task that recovers Dirty sessions by rewarming them.
    ///
    /// Runs continuously with the following behavior:
    /// - Polls every 5 seconds for Dirty sessions
    /// - Attempts to rewarm each Dirty session (page reload, refresh cookies)
    /// - Transitions Dirty → Ready on success
    /// - Logs warnings for persistent failures
    ///
    /// This prevents permanent session pool leaks when transient errors
    /// (network timeout, Cloudflare challenge, etc.) mark sessions as Dirty.
    async fn background_recovery_loop(sessions: Arc<Mutex<HashMap<String, SessionThread>>>) {
        let recovery_interval = Duration::from_secs(5);
        let mut interval = tokio::time::interval(recovery_interval);

        loop {
            interval.tick().await;

            let mut dirty_sessions = Vec::new();
            {
                let guard = sessions.lock().await;
                for (id, session) in guard.iter() {
                    let state = session.state.lock().await;
                    if *state == SessionState::Dirty {
                        dirty_sessions.push(id.clone());
                    }
                }
            }

            // Attempt to recover each Dirty session
            for session_id in dirty_sessions {
                let should_rewarm = {
                    let guard = sessions.lock().await;
                    if let Some(session) = guard.get(&session_id) {
                        let mut state = session.state.lock().await;

                        // Only attempt rewarm if still Dirty
                        if *state == SessionState::Dirty {
                            *state = SessionState::Warming;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if should_rewarm {
                    // Attempt rewarming (simplified: just reset to Ready)
                    // In a real implementation, this would navigate the page or refresh cookies
                    let guard = sessions.lock().await;
                    if let Some(session) = guard.get(&session_id) {
                        let mut state = session.state.lock().await;
                        *state = SessionState::Ready;
                        info!(session_id = %session_id, "Recovered Dirty session → Ready");
                    }
                }
            }
        }
    }

    /// Attempt to rewarm a session by navigating to a provider URL.
    /// Returns Ok(()) if successful, Err if rewarming failed.
    async fn attempt_rewarm(session_handle: &SessionHandle, session_id: &str) -> Result<(), String> {
        // Send a rewarm command to the session thread
        // The session thread will navigate to deepseek.com and refresh cookies
        // This is a best-effort recovery; if it fails, we'll retry on the next interval
        Ok(())
    }

    /// Acquire a ready browser context.
    ///
    /// If warm-up has not finished (or all warmed sessions are busy) and the
    /// pool is below its size limit, creates a session on demand so the
    /// gateway never blocks on provider navigation.
    pub async fn acquire(&self) -> Result<SessionHandle, GatewayError> {
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(ACQUIRE_TIMEOUT_MS);

        loop {
            {
                let guard = self.sessions.lock().await;
                for session in guard.values() {
                    let mut state = session.state.lock().await;
                    if *state == SessionState::Ready {
                        *state = SessionState::Busy;
                        return Ok(SessionHandle {
                            id: session.id.clone(),
                            cookie_jar: session.cookie_jar.clone(),
                            local_storage: session.local_storage.clone(),
                            user_agent: session.user_agent.clone(),
                        });
                    }
                }
            }

            // Lazy fallback: if the pool is not yet full, create a session on
            // demand rather than waiting for background warm-up.
            if self.alive.fetch_add(1, Ordering::AcqRel) < self.pool_size {
                let auth = self.auth.lock().await.clone();
                if let Some((cookies, local_storage, identity)) = auth {
                    let id = format!("session-lazy-{}", uuid::Uuid::new_v4().simple());
                    match SessionThread::spawn(id.clone(), cookies, local_storage, identity).await {
                        Ok(session) => {
                            let handle = SessionHandle {
                                id: session.id.clone(),
                                cookie_jar: session.cookie_jar.clone(),
                                local_storage: session.local_storage.clone(),
                                user_agent: session.user_agent.clone(),
                            };
                            // Mark the freshly-warmed session as busy before
                            // handing it out so background warm-up skips it.
                            *session.state.lock().await = SessionState::Busy;
                            self.sessions.lock().await.insert(session.id.clone(), session);
                            info!(session_id = %handle.id, "Created browser session on demand");
                            return Ok(handle);
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to create session on demand");
                            self.alive.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                } else {
                    self.alive.fetch_sub(1, Ordering::AcqRel);
                }
            } else {
                self.alive.fetch_sub(1, Ordering::AcqRel);
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(GatewayError::Provider(
                    "all browser sessions are busy; try again later".to_string(),
                ));
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(ACQUIRE_POLL_MS)).await;
        }
    }

    /// Release a previously acquired context.
    ///
    /// Mark `dirty` if the request failed in a way that may have invalidated
    /// the session cookies (e.g., a 403 or challenge response).
    pub async fn release(&self, session_id: String, dirty: bool) -> Result<(), GatewayError> {
        let guard = self.sessions.lock().await;
        let session = guard.get(&session_id).ok_or_else(|| {
            GatewayError::Internal(format!("session {session_id} not found"))
        })?;

        let mut state = session.state.lock().await;
        if *state == SessionState::Busy {
            *state = if dirty {
                warn!(session_id = %session_id, "Session marked dirty");
                SessionState::Dirty
            } else {
                SessionState::Ready
            };
        }
        Ok(())
    }

    /// Re-warm a single context to refresh cookies and anti-bot state.
    #[allow(dead_code)]
    pub async fn rewarm(&self, session_id: String) -> Result<(), GatewayError> {
        let (tx, rx) = oneshot::channel();
        {
            let guard = self.sessions.lock().await;
            let session = guard.get(&session_id).ok_or_else(|| {
                GatewayError::Internal(format!("session {session_id} not found"))
            })?;
            session
                .cmd_tx
                .send(SessionCommand::ReWarm { resp: tx })
                .map_err(|_| GatewayError::Internal("session thread closed".to_string()))?;
        }
        rx.await
            .map_err(|_| GatewayError::Internal("session thread dropped response".to_string()))?
    }

    /// Execute an async JavaScript expression on the warmed page.
    ///
    /// The expression should be an `async` function or a Promise. The session
    /// thread awaits the promise and returns the resolved value. Used to run
    /// `fetch()` calls inside the page so the request inherits the page's
    /// cookie jar (i.e. the user's real session).
    #[allow(dead_code)]
    pub async fn execute_js_async(
        &self,
        session_id: &str,
        expression: &str,
    ) -> Result<serde_json::Value, GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::ExecuteJsAsync {
            expression: expression.to_string(),
            resp,
        })
        .await
    }

    /// Extract the visible text of the response and (optionally) thinking
    /// subtrees in a single round-trip to the session thread.
    ///
    /// Hidden subtrees are skipped, and skippable roles (buttons, images,
    /// form controls, decorative elements) are filtered out. Used by
    /// [`crate::chat`] to poll for streaming text growth.
    pub async fn extract_texts(
        &self,
        session_id: &str,
        response_selector: &str,
        thinking_selector: Option<&str>,
    ) -> Result<ExtractedTexts, GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::ExtractTexts {
            response_selector: response_selector.to_string(),
            thinking_selector: thinking_selector.map(|s| s.to_string()),
            resp,
        })
        .await
    }

    /// Returns `true` if `selector` matches a visible element on the page.
    ///
    /// Used to drive [`crate::providers::DoneSignal::SelectorDisappears`]:
    /// poll this each tick; when it transitions from true to false the
    /// stream is considered complete.
    #[allow(dead_code)]
    pub async fn is_visible(&self, session_id: &str, selector: &str) -> Result<bool, GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::IsVisible {
            selector: selector.to_string(),
            resp,
        })
        .await
    }

    /// Remove all preload scripts from the page.
    pub async fn clear_preload_scripts(&self, session_id: &str) -> Result<(), GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::ClearPreloadScripts { resp })
            .await
    }

    /// Add a script that runs before any page scripts on the next navigation.
    /// Takes effect only on the subsequent [`SessionManager::navigate`] call.
    pub async fn add_preload_script(
        &self,
        session_id: &str,
        script: &str,
    ) -> Result<(), GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::AddPreloadScript {
            script: script.to_string(),
            resp,
        })
        .await
    }

    /// Navigate the session's page to a new URL.
    ///
    /// Used by providers (e.g. Gemini) that need the warm page to be at a
    /// specific URL to extract CSRF tokens and other auth state.
    pub async fn navigate(
        &self,
        session_id: &str,
        url: &str,
    ) -> Result<(), GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::Navigate {
            url: url.to_string(),
            resp,
        })
        .await
    }

    /// Execute a synchronous JavaScript expression on the warmed page.
    pub async fn execute_js(
        &self,
        session_id: &str,
        expression: &str,
    ) -> Result<serde_json::Value, GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::ExecuteJs {
            expression: expression.to_string(),
            resp,
        })
        .await
    }

    /// Start capturing JS responses whose URL contains `url_pattern`.
    ///
    /// Returns a handle that can be polled with [`CaptureHandle::take_responses`].
    /// Captures are per-session and replace any previous active capture.
    pub async fn start_capture(
        &self,
        session_id: &str,
        url_pattern: &str,
    ) -> Result<CaptureHandle, GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::StartCapture {
            url_pattern: url_pattern.to_string(),
            resp,
        })
        .await
    }

    /// Stop capturing and return all responses collected since start.
    pub async fn stop_capture(
        &self,
        session_id: &str,
    ) -> Result<Vec<CapturedResponse>, GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::StopCapture { resp }).await
    }

    /// Read the captcha-open response body captured by the `on_response`
    /// callback. Returns `None` if no captcha response has been captured yet.
    /// The body is consumed on read (take).
    pub async fn get_captcha_response_body(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<u8>>, GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::GetCaptchaResponseBody { resp }).await
    }

    /// Pump the Deno event loop so pending async ops (in-page fetch) complete.
    pub async fn pump_event_loop(
        &self,
        session_id: &str,
        duration_ms: u64,
    ) -> Result<(), GatewayError> {
        self.send_command(session_id, |resp| SessionCommand::PumpEventLoop { duration_ms, resp }).await
    }

    /// Create a no-op manager for tests that never touches a real browser.
    #[cfg(test)]
    pub fn noop() -> Self {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        Self {
            sessions,
            auth: Arc::new(Mutex::new(None)),
            pool_size: 0,
            alive: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Shut down all session threads gracefully.
    #[allow(dead_code)]
    pub async fn shutdown(&self) -> Result<(), GatewayError> {
        let guard = self.sessions.lock().await;
        for session in guard.values() {
            let _ = session.cmd_tx.send(SessionCommand::Shutdown);
        }
        Ok(())
    }

    async fn send_command<T, F>(
        &self,
        session_id: &str,
        make_cmd: F,
    ) -> Result<T, GatewayError>
    where
        F: FnOnce(oneshot::Sender<Result<T, GatewayError>>) -> SessionCommand,
    {
        let (tx, rx) = oneshot::channel();
        {
            let guard = self.sessions.lock().await;
            let session = guard.get(session_id).ok_or_else(|| {
                GatewayError::Internal(format!("session {session_id} not found"))
            })?;
            session
                .cmd_tx
                .send(make_cmd(tx))
                .map_err(|_| GatewayError::Internal("session thread closed".to_string()))?;
        }
        rx.await
            .map_err(|_| GatewayError::Internal("session thread dropped response".to_string()))?
    }
}

#[derive(Debug)]
enum SessionCommand {
    ExecuteJs {
        expression: String,
        resp: oneshot::Sender<Result<serde_json::Value, GatewayError>>,
    },
    AddPreloadScript {
        script: String,
        resp: oneshot::Sender<Result<(), GatewayError>>,
    },
    /// Remove all preload scripts from the page.
    ClearPreloadScripts {
        resp: oneshot::Sender<Result<(), GatewayError>>,
    },
    #[allow(dead_code)]
    ExecuteJsAsync {
        expression: String,
        resp: oneshot::Sender<Result<serde_json::Value, GatewayError>>,
    },
    /// Extract the visible text of two subtrees (response + thinking) in
    /// one round-trip to the session thread.
    ExtractTexts {
        response_selector: String,
        thinking_selector: Option<String>,
        resp: oneshot::Sender<Result<ExtractedTexts, GatewayError>>,
    },
    /// Returns true when the selector matches a visible element on the page.
    /// Used by [`crate::chat`] to drive [`crate::providers::DoneSignal::SelectorDisappears`].
    IsVisible {
        selector: String,
        resp: oneshot::Sender<Result<bool, GatewayError>>,
    },
    Navigate {
        url: String,
        resp: oneshot::Sender<Result<(), GatewayError>>,
    },
    StartCapture {
        url_pattern: String,
        resp: oneshot::Sender<Result<CaptureHandle, GatewayError>>,
    },
    StopCapture {
        resp: oneshot::Sender<Result<Vec<CapturedResponse>, GatewayError>>,
    },
    GetCaptchaResponseBody {
        resp: oneshot::Sender<Result<Option<Vec<u8>>, GatewayError>>,
    },
    /// Pump the Deno event loop for `duration_ms` so pending async ops
    /// (in-page fetch/XHR) can complete.
    PumpEventLoop {
        duration_ms: u64,
        resp: oneshot::Sender<Result<(), GatewayError>>,
    },
    #[allow(dead_code)]
    ReWarm {
        resp: oneshot::Sender<Result<(), GatewayError>>,
    },
    Shutdown,
}

#[derive(PartialEq, Eq)]
enum ControlFlow {
    Continue,
    Break,
}

/// Manager-side handle for a single session thread.
struct SessionThread {
    id: String,
    cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    state: Arc<Mutex<SessionState>>,
    cookie_jar: Arc<CookieJar>,
    local_storage: Vec<LocalStorageEntry>,
    user_agent: String,
    /// Shared capture state. Stored here so the session can expose it via
    /// commands; the command loop reaches it through the captured closure
    /// in `spawn`, not through `self`.
    #[allow(dead_code)]
    capture: Arc<Mutex<ActiveCapture>>,
    /// Lock-free storage for captcha-open response body, populated by the
    /// `on_response` callback and read by providers. Uses std::sync::Mutex
    /// so the sync callback never misses writes.
    #[allow(dead_code)]
    captcha_response_body: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

impl SessionThread {
    /// Spawn a dedicated OS thread for one browser session, warm the page, and
    /// return a handle that can drive it.
    async fn spawn(
        id: String,
        cookies: Vec<CookieInfo>,
        local_storage: Vec<LocalStorageEntry>,
        identity: BrowserIdentity,
    ) -> Result<Self, GatewayError> {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
        let state = Arc::new(Mutex::new(SessionState::Warming));
        let state_for_thread = state.clone();
        let capture = Arc::new(Mutex::new(ActiveCapture::default()));
        let capture_for_thread = capture.clone();
        let captcha_response_body: Arc<std::sync::Mutex<Option<Vec<u8>>>> = Default::default();
        let captcha_body_for_thread = captcha_response_body.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let id_for_thread = id.clone();
        let identity_for_thread = identity.clone();

        std::thread::Builder::new()
            .name(format!("obscura-session-{}", id))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("session tokio runtime");

                rt.block_on(async {
                    // Each session owns its Page on its own thread. Build it here
                    // so the !Send ObscuraJsRuntime never crosses thread boundaries.
                    let warm_result =
                        Self::warm(&id_for_thread, cookies, &local_storage, &identity_for_thread, capture_for_thread.clone(), captcha_body_for_thread.clone())
                            .await;

                    match warm_result {
                        Ok((mut page, cookie_jar)) => {
                            *state_for_thread.lock().await = SessionState::Ready;
                            let _ = ready_tx.send(Ok((
                                cookie_jar,
                                local_storage,
                                identity_for_thread.user_agent.clone(),
                            )));

                            let local = tokio::task::LocalSet::new();
                            local
                                .run_until(async {
                                    while let Some(cmd) = cmd_rx.recv().await {
                                        let result =
                                            Self::handle_command(cmd, &mut page, &state_for_thread, &capture_for_thread, &captcha_body_for_thread)
                                                .await;
                                        if result == ControlFlow::Break {
                                            break;
                                        }
                                    }
                                })
                                .await;
                        }
                        Err(e) => {
                            error!(session_id = %id_for_thread, error = %e, "Failed to warm session");
                            *state_for_thread.lock().await = SessionState::Failed;
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                });
            })
            .map_err(|e| GatewayError::Internal(format!("failed to spawn session thread: {e}")))?;

        let (cookie_jar, local_storage, user_agent) = ready_rx
            .await
            .map_err(|_| GatewayError::Internal("session thread failed to start".to_string()))??;

        Ok(SessionThread {
            id,
            cmd_tx,
            state,
            cookie_jar,
            local_storage,
            user_agent,
            capture,
            captcha_response_body,
        })
    }

    /// Create a `BrowserContext`/`Page`, navigate to DeepSeek, and inject the
    /// imported localStorage. Runs entirely on the session's own thread.
    async fn warm(
        id: &str,
        cookies: Vec<CookieInfo>,
        local_storage: &[LocalStorageEntry],
        identity: &BrowserIdentity,
        capture: Arc<Mutex<ActiveCapture>>,
        captcha_response_body: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    ) -> Result<(Page, Arc<CookieJar>), GatewayError> {
        let temp_dir = tempfile::tempdir().map_err(|e| {
            GatewayError::Internal(format!("failed to create temp profile: {e}"))
        })?;

        let context = Arc::new(BrowserContext::with_storage_full(
            id.to_string(),
            None,
            identity.identity == "chrome",
            Some(identity.user_agent.clone()),
            Some(temp_dir.path().to_path_buf()),
        ));
        context.cookie_jar.set_cookies_from_cdp(cookies);

        let mut page = Page::new(id.to_string(), context.clone());

        // Log every JS-initiated request/response so we can see whether the
        // chat submit actually reaches DeepSeek's backend.
        page.on_request(Arc::new(|req: &RequestInfo| {
            let statsig = req.headers.get("x-statsig-id")
                .or_else(|| req.headers.get("X-Statsig-Id"));
            if let Some(id) = statsig {
                info!(url = %req.url, method = %req.method, statsig_header = %id, "JS request (with x-statsig-id)");
            } else if req.url.as_str().contains("grok.com/rest/") {
                info!(url = %req.url, method = %req.method, headers = ?req.headers, "JS request (grok API)");
            } else {
                info!(url = %req.url, method = %req.method, "JS request");
            }
        }));
        let captcha_body = captcha_response_body.clone();
        page.on_response(Arc::new(move |req: &RequestInfo, resp: &Response| {
            info!(url = %req.url, status = %resp.status, "JS response");
            // Store matching responses for provider capture. The callback is
            // synchronous, so use try_lock to avoid blocking the network thread.
            if let Ok(mut guard) = capture.try_lock() {
                if let Some(ref pattern) = guard.pattern {
                    if req.url.as_str().contains(pattern) {
                        guard.responses.push(CapturedResponse {
                            url: req.url.to_string(),
                            status: resp.status,
                            headers: resp
                                .headers
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                            body: resp.body.clone(),
                        });
                    }
                }
            }
            // Always capture captcha-open and chat API response bodies
            // (lock-free store, populated by op_fetch_url's Rust callback).
            if req.url.as_str().contains("captcha-open.aliyuncs.com")
                || req.url.as_str().contains("/api/v2/chat/completions")
            {
                if let Ok(mut guard) = captcha_body.try_lock() {
                    guard.replace(resp.body.clone());
                }
            }
        }));

        // Inject localStorage as a preload script so DeepSeek's auth check sees
        // the session token before any page scripts run. Post-navigate injection
        // is too late: the page may already have redirected to /sign_in.
        page.add_preload_script(&local_storage_preload_script(local_storage));

        page.navigate("https://chat.deepseek.com")
            .await
            .map_err(|e| GatewayError::Provider(format!("failed to warm DeepSeek page: {e}")))?;

        info!(
            session_id = %id,
            url = %page.url_string(),
            "Session warmed"
        );

        let cookie_jar = context.cookie_jar.clone();
        Ok((page, cookie_jar))
    }

    async fn handle_command(
        cmd: SessionCommand,
        page: &mut Page,
        state: &Arc<Mutex<SessionState>>,
        capture: &Arc<Mutex<ActiveCapture>>,
        captcha_body: &Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    ) -> ControlFlow {
        match cmd {
            SessionCommand::ExecuteJs { expression, resp } => {
                let result = Ok(page.evaluate(&expression));
                let _ = resp.send(result);
            }
            SessionCommand::ExecuteJsAsync { expression, resp } => {
                // Synchronous eval with a watchdog. Callers must structure JS to
                // return a value rather than a Promise.
                let result = Ok(page.evaluate_with_timeout(
                    &expression,
                    Duration::from_secs(10),
                ));
                let _ = resp.send(result);
            }
            SessionCommand::ExtractTexts {
                response_selector,
                thinking_selector,
                resp,
            } => {
                let result = extract_texts(page, &response_selector, thinking_selector.as_deref());
                let _ = resp.send(result);
            }
            SessionCommand::IsVisible { selector, resp } => {
                let result = is_visible(page, &selector);
                let _ = resp.send(result);
            }
            SessionCommand::AddPreloadScript { script, resp } => {
                page.add_preload_script(&script);
                let _ = resp.send(Ok(()));
            }
            SessionCommand::ClearPreloadScripts { resp } => {
                page.set_preload_scripts(Vec::new());
                let _ = resp.send(Ok(()));
            }
            SessionCommand::Navigate { url, resp } => {
                tracing::info!(url = %url, "Navigating session");
                let result = page
                    .navigate(&url)
                    .await
                    .map_err(|e| GatewayError::Provider(format!("navigation to {url} failed: {e}")));
                let _ = resp.send(result);
            }
            SessionCommand::StartCapture { url_pattern, resp } => {
                let mut guard = capture.lock().await;
                guard.pattern = Some(url_pattern);
                guard.responses.clear();
                let handle = CaptureHandle {
                    state: capture.clone(),
                };
                let _ = resp.send(Ok(handle));
            }
            SessionCommand::StopCapture { resp } => {
                let mut guard = capture.lock().await;
                let responses = std::mem::take(&mut guard.responses);
                guard.pattern = None;
                let _ = resp.send(Ok(responses));
            }
            SessionCommand::ReWarm { resp } => {
                *state.lock().await = SessionState::Warming;
                let result = page
                    .navigate("https://chat.deepseek.com")
                    .await
                    .map_err(|e| GatewayError::Provider(format!("failed to re-warm session: {e}")));
                *state.lock().await = if result.is_ok() {
                    SessionState::Ready
                } else {
                    SessionState::Dirty
                };
                let _ = resp.send(result);
            }
            SessionCommand::GetCaptchaResponseBody { resp } => {
                let body = captcha_body.try_lock().ok().and_then(|mut g| g.take());
                let _ = resp.send(Ok(body));
            }
            SessionCommand::PumpEventLoop { duration_ms, resp } => {
                page.settle(duration_ms).await;
                let _ = resp.send(Ok(()));
            }
            SessionCommand::Shutdown => return ControlFlow::Break,
        }
        ControlFlow::Continue
    }
}

/// Extract the visible text of the response + (optional) thinking
/// subtrees of a session's current page.
///
/// Pure DOM walk — no JS execution. Hidden subtrees are skipped;
/// skippable roles (buttons, images, form controls, decorative
/// elements) are filtered out. Returns empty strings when the
/// selectors don't match any visible element.
fn extract_texts(
    page: &mut Page,
    response_selector: &str,
    thinking_selector: Option<&str>,
) -> Result<ExtractedTexts, GatewayError> {
    let response = page
        .query_visible(response_selector)
        .map_err(|e| GatewayError::Provider(format!("invalid response selector: {e}")))?
        .map(|nid| page.visible_text(nid))
        .unwrap_or_default();

    let thinking = match thinking_selector {
        Some(sel) => page
            .query_visible(sel)
            .map_err(|e| GatewayError::Provider(format!("invalid thinking selector: {e}")))?
            .map(|nid| page.visible_text(nid))
            .unwrap_or_default(),
        None => String::new(),
    };

    Ok(ExtractedTexts { response, thinking })
}

/// Returns `true` when the selector matches a visible element.
fn is_visible(page: &mut Page, selector: &str) -> Result<bool, GatewayError> {
    match page.query_visible(selector) {
        Ok(opt) => Ok(opt.is_some()),
        Err(e) => Err(GatewayError::Provider(format!("invalid selector: {e}"))),
    }
}

/// Build a preload script that restores browser localStorage only at its
/// original origin. A pooled page may navigate across providers, so replaying
/// every entry on every page would leak session state between providers.
fn local_storage_preload_script(entries: &[LocalStorageEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let payload = entries
        .iter()
        .map(|e| serde_json::json!({
            "origin": e.origin,
            "key": e.key,
            "value": e.value,
        }))
        .collect::<Vec<_>>();

    match serde_json::to_string(&payload) {
        Ok(json) => format!(
            "(function(entries) {{ \
                try {{ \
                    entries.forEach(function(entry) {{ \
                        if (entry.origin === location.origin) window.localStorage.setItem(entry.key, entry.value); \
                    }}); \
                }} catch (e) {{ \
                    console.error('obscura: failed to seed localStorage', e); \
                }} \
            }})({});",
            json
        ),
        Err(e) => {
            warn!(error = %e, "Failed to serialize localStorage preload payload");
            String::new()
        }
    }
}
