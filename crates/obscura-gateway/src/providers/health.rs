//! Provider health: error classification, circuit breaking, and rate limiting.
//!
//! The gateway gates outbound requests through [`ProviderHealthRegistry`]
//! (a per-provider circuit breaker with half-open canary probes) and
//! [`ProviderRateLimiter`] (per-provider max concurrency + requests/min).
//! Failures are classified with [`classify_error`] using evidence from the
//! provider's own response: status codes and error markers that the adapters
//! surface verbatim in provider error messages.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use futures::Stream;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, info, warn};

use crate::error::GatewayError;
use crate::models::ChatCompletionChunk;

/// Maximum time `acquire` will wait for a rate-limit token before failing.
pub const MAX_RATE_WAIT: Duration = Duration::from_secs(30);

/// Terminal taxonomy for a single failed provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The provider has no usable credentials at all (no session cookie).
    AuthMissing,
    /// Credentials are present but expired or rejected (`401`, `token_expired`).
    AuthExpired,
    /// The provider served a challenge (captcha / anti-bot / 403 verify page).
    Challenge,
    /// The provider rate-limited the request (`429`, token bucket exhausted).
    RateLimit,
    /// The provider quota/plan is exhausted or the account is locked.
    Quota,
    /// The provider's wire protocol drifted from what the adapter expects.
    Drift,
    /// The response body could not be decoded into the expected shape.
    Parse,
    /// Transport-level failure (timeout, connection reset).
    Network,
    /// The requested model is not exposed by the provider.
    ModelUnsupported,
    /// Anything that defies the taxonomy; treated as a transient blip.
    Other,
}

impl ErrorClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ErrorClass::AuthMissing => "auth_missing",
            ErrorClass::AuthExpired => "auth_expired",
            ErrorClass::Challenge => "challenge_required",
            ErrorClass::RateLimit => "rate_limited",
            ErrorClass::Quota => "quota_exhausted",
            ErrorClass::Drift => "protocol_drift",
            ErrorClass::Parse => "response_parse",
            ErrorClass::Network => "network",
            ErrorClass::ModelUnsupported => "model_unsupported",
            ErrorClass::Other => "other",
        }
    }

    /// Whether a failure warrants circuit-opening pressure. `Parse` failures
    /// only gain pressure once repeated (a single bad body is transient, an
    /// unparseable stream three times in a row is a real upstream change);
    /// `Other` never opens.
    fn breaks_circuit(self, consecutive: u32) -> bool {
        match self {
            ErrorClass::Parse => consecutive >= 3,
            ErrorClass::Other => false,
            _ => true,
        }
    }

    /// Cooldown applied before the next canary probe is allowed.
    fn cooldown(self) -> Duration {
        match self {
            ErrorClass::AuthMissing => Duration::from_secs(10 * 60),
            ErrorClass::AuthExpired => Duration::from_secs(12 * 60 * 60),
            ErrorClass::Challenge => Duration::from_secs(20 * 60),
            ErrorClass::RateLimit => Duration::from_secs(60),
            ErrorClass::Quota => Duration::from_secs(6 * 60 * 60),
            ErrorClass::Drift => Duration::from_secs(30 * 60),
            ErrorClass::Network => Duration::from_secs(5 * 60),
            ErrorClass::Parse | ErrorClass::Other => Duration::from_secs(30),
            ErrorClass::ModelUnsupported => Duration::from_secs(60),
        }
    }
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Long-lived health state of one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    /// Temporarily degraded but accepting traffic (half-open canary).
    Degraded,
    AuthMissing,
    AuthExpired,
    ChallengeRequired,
    RateLimited,
    QuotaExhausted,
    ProtocolDrift,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthState::Healthy => "healthy",
            HealthState::Degraded => "degraded",
            HealthState::AuthMissing => "auth_missing",
            HealthState::AuthExpired => "auth_expired",
            HealthState::ChallengeRequired => "challenge_required",
            HealthState::RateLimited => "rate_limited",
            HealthState::QuotaExhausted => "quota_exhausted",
            HealthState::ProtocolDrift => "protocol_drift",
        }
    }

    /// Human actionable hint surfaced with gate errors.
    fn remediation(self) -> &'static str {
        match self {
            HealthState::AuthMissing => {
                "the provider session is missing; import a browser profile with a logged-in provider first"
            }
            HealthState::AuthExpired => {
                "the provider session expired; log in again in the source browser and re-import"
            }
            HealthState::ChallengeRequired => {
                "the provider is showing an anti-bot captcha; solve it in the source browser and re-import"
            }
            HealthState::RateLimited => {
                "the provider is rate limiting requests; wait and retry"
            }
            HealthState::QuotaExhausted => {
                "the provider account quota is exhausted; top up or switch models"
            }
            HealthState::ProtocolDrift => {
                "the provider changed its API surface; this gateway build needs a provider update"
            }
            _ => "temporary provider disturbance; retry shortly",
        }
    }
}

impl From<ErrorClass> for HealthState {
    fn from(class: ErrorClass) -> Self {
        match class {
            ErrorClass::AuthMissing => HealthState::AuthMissing,
            ErrorClass::AuthExpired => HealthState::AuthExpired,
            ErrorClass::Challenge => HealthState::ChallengeRequired,
            ErrorClass::RateLimit => HealthState::RateLimited,
            ErrorClass::Quota => HealthState::QuotaExhausted,
            ErrorClass::Drift => HealthState::ProtocolDrift,
            ErrorClass::Network => HealthState::Degraded,
            ErrorClass::Parse | ErrorClass::Other => HealthState::Degraded,
            ErrorClass::ModelUnsupported => HealthState::Healthy,
        }
    }
}

#[derive(Debug, Clone)]
struct Status {
    state: HealthState,
    consecutive_failures: u32,
    /// When the current lockout/cooldown was armed; `None` when accepting traffic.
    lockout_until: Option<Instant>,
}

impl Status {
    fn healthy() -> Self {
        Status {
            state: HealthState::Healthy,
            consecutive_failures: 0,
            lockout_until: None,
        }
    }
}

/// Per-provider circuit breaker.
///
/// Requests pass through [`gate_new_request`][Self::gate_new_request] unless a
/// lockout is armed. A lockout expires after the class cooldown, at which
/// point the provider enters a half-open [`HealthState::Degraded`] state and
/// the next request becomes a canary probe: success heals it, a repeat
/// failure re-arms the lockout.
#[derive(Clone, Default)]
pub struct ProviderHealthRegistry {
    inner: Arc<Mutex<HashMap<String, Status>>>,
}

impl ProviderHealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    async fn status(&self, provider: &str) -> Status {
        self.inner
            .lock()
            .await
            .get(provider)
            .cloned()
            .unwrap_or_else(Status::healthy)
    }

    /// Gate a new outbound request for `provider`.
    ///
    /// Returns `Ok` when the request may proceed (healthy, degraded, or the
    /// lockout has expired and this request is the canary). Returns a
    /// descriptive error while a lockout is still armed.
    pub async fn gate_new_request(&self, provider: &str) -> Result<Priority, GatewayError> {
        let mut map = self.inner.lock().await;
        let entry = map.entry(provider.to_string()).or_insert_with(Status::healthy);

        let Some(until) = entry.lockout_until else {
            return Ok(Priority::UserLive);
        };
        if until > Instant::now() {
            let remaining = until.saturating_duration_since(Instant::now());
            let err = GatewayError::Provider(format!(
                "provider '{provider}' is temporarily unavailable ({}): {}",
                entry.state.as_str(),
                entry.state.remediation()
            ));
            warn!(provider, remaining_ms = remaining.as_millis(), "gate rejected request");
            return Err(err);
        }

        // Lockout expired: half-open canary probe.
        info!(provider, "circuit half-open; canary probe allowed");
        entry.state = HealthState::Degraded;
        entry.lockout_until = None;
        Ok(Priority::Canary)
    }

    /// Record a successful provider request; heals the circuit.
    pub async fn record_success(&self, provider: &str) {
        let mut map = self.inner.lock().await;
        let entry = map.entry(provider.to_string()).or_insert_with(Status::healthy);
        if entry.state != HealthState::Healthy {
            info!(provider, "provider recovered");
        }
        *entry = Status::healthy();
    }

    /// Record a failed provider request; arms a lockout unless the class does
    /// not carry circuit-opening pressure. `Parse` gains pressure only after
    /// the third consecutive failure (a single bad body is transient).
    pub async fn record_error(&self, provider: &str, class: ErrorClass) {
        let mut map = self.inner.lock().await;
        let entry = map.entry(provider.to_string()).or_insert_with(Status::healthy);
        entry.consecutive_failures += 1;
        if !class.breaks_circuit(entry.consecutive_failures) {
            if matches!(class, ErrorClass::Parse) {
                // Repeated parse failures are upstream drift in disguise:
                // surface the degraded state early, but keep accepting
                // canary traffic until the third strike opens the circuit.
                entry.state = HealthState::Degraded;
                debug!(provider, failures = entry.consecutive_failures, "degraded by parse failures");
            }
            return;
        }
        let state = HealthState::from(class);
        let prev = entry.state;
        if entry.state == state {
            // Escalate the lockout after repeated same-class failures so the
            // canary isn't hammered every cooldown window.
            let cooldown = class.cooldown() * entry.consecutive_failures.min(6);
            entry.lockout_until = Some(Instant::now() + cooldown);
            warn!(provider, class = %class, failures = entry.consecutive_failures, cooldown_ms = cooldown.as_millis(), "provider circuit opened (escalated)");
        } else {
            entry.state = state;
            entry.lockout_until = Some(Instant::now() + class.cooldown());
            warn!(provider, class = %class, "provider circuit opened ({} → {})", prev.as_str(), state.as_str());
        }
    }

    /// Snapshot for the `/health` endpoint and observability.
    pub async fn snapshot(&self) -> Vec<ProviderHealthSnapshot> {
        let map = self.inner.lock().await;
        let mut out: Vec<ProviderHealthSnapshot> = map
            .iter()
            .map(|(name, status)| ProviderHealthSnapshot {
                provider: name.clone(),
                state: status.state,
                consecutive_failures: status.consecutive_failures,
                locked_out_secs: status
                    .lockout_until
                    .map(|t| t.saturating_duration_since(Instant::now()).as_secs())
                    .unwrap_or_default(),
            })
            .collect();
        out.sort_by(|a, b| a.provider.cmp(&b.provider));
        out
    }
}

/// A single provider's health snapshot for reporting.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderHealthSnapshot {
    pub provider: String,
    pub state: HealthState,
    pub consecutive_failures: u32,
    /// Remaining lockout in seconds; zero when not locked out.
    pub locked_out_secs: u64,
}

impl serde::Serialize for HealthState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Per-provider rate limiting configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RateConfig {
    /// Maximum concurrent in-flight requests to the provider.
    pub max_concurrency: usize,
    /// Sustained token refill rate, in tokens per minute.
    pub requests_per_minute: u32,
    /// Burst capacity above the sustained rate.
    pub burst: u32,
    /// Rolling hourly request cap (0 disables the cap).
    pub messages_per_hour: u32,
    /// Cooldown applied after a provider 429; requests reject fast.
    pub cooldown_after_429: Duration,
}

impl Default for RateConfig {
    fn default() -> Self {
        RateConfig {
            max_concurrency: 4,
            requests_per_minute: 60,
            burst: 4,
            messages_per_hour: 600,
            cooldown_after_429: Duration::from_secs(60),
        }
    }
}

/// Rate config override for a provider. Checks the manifest registry first,
/// then falls back to hardcoded overrides for providers not yet using manifests.
pub fn rate_config_for(provider: &str) -> RateConfig {
    if let Some(manifest) = super::manifest::find_manifest(provider) {
        return manifest.rate_config();
    }
    RateConfig::default()
}

/// Prioritization for queued concurrency slots, highest first. User-facing
/// traffic outranks maintenance traffic (canary probes, research).
///
/// `Continuation` and `ResearchProbe` are reserved for internal traffic
/// classes that do not exist yet (session-continuation retries and
/// scheduled research probes); they keep their ladder positions so existing
/// callers never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priority {
    UserLive,
    #[allow(dead_code)]
    Continuation,
    Canary,
    #[allow(dead_code)]
    ResearchProbe,
}

impl Priority {
    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug)]
struct BucketInner {
    tokens: f64,
    capacity: f64,
    fill_per_sec: f64,
    last_refill: Instant,
    cooldown_until: Option<Instant>,
    /// Rolling window of request timestamps for the hourly cap.
    hourly: VecDeque<Instant>,
}

impl BucketInner {
    fn new(cfg: RateConfig) -> Self {
        BucketInner {
            tokens: f64::from(cfg.burst),
            capacity: f64::from(cfg.burst),
            fill_per_sec: f64::from(cfg.requests_per_minute) / 60.0,
            last_refill: Instant::now(),
            cooldown_until: None,
            hourly: VecDeque::new(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.fill_per_sec).min(self.capacity);
            self.last_refill = now;
        }
    }

    /// Prune hourly-window entries older than one hour and report how many
    /// requests were made in the current window.
    fn hourly_count(&mut self) -> usize {
        let cutoff = Instant::now() - HOURLY_WINDOW;
        while self.hourly.front().is_some_and(|t| *t <= cutoff) {
            self.hourly.pop_front();
        }
        self.hourly.len()
    }
}

const HOURLY_WINDOW: Duration = Duration::from_secs(3600);

/// A wait-list to a bounded pool of concurrency slots, serviced strictly by
/// [`Priority`] (FIFO within a priority). Slots are released when a
/// [`RateLimitPermit`] drops; release hands the slot to the highest-priority
/// waiter via a oneshot, so there are no wake storms and no busy loops.
#[derive(Debug)]
struct ConcurrencyGate {
    inner: std::sync::Mutex<GateInner>,
    next_id: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct GateInner {
    available: usize,
    queues: [VecDeque<Waiter>; 4],
}

struct Waiter {
    id: u64,
    tx: oneshot::Sender<()>,
}

impl std::fmt::Debug for Waiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waiter").field("id", &self.id).finish()
    }
}

impl ConcurrencyGate {
    fn new(slots: usize) -> Self {
        ConcurrencyGate {
            inner: std::sync::Mutex::new(GateInner {
                available: slots,
                queues: Default::default(),
            }),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, GateInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queue for a slot, blocking until `deadline`. Returns `Ok` once granted
    /// (or if granted concurrently just before the deadline expired, so a
    /// slot is never leaked). Returns `Err` when the deadline passes while
    /// still queued; the wait-list entry is cancelled cleanly.
    async fn acquire(&self, priority: Priority, deadline: Instant) -> Result<(), GatewayError> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let rx = {
            let mut inner = self.lock_inner();
            if inner.available > 0 {
                inner.available -= 1;
                return Ok(());
            }
            let (tx, rx) = oneshot::channel();
            inner.queues[priority.index()].push_back(Waiter { id, tx });
            rx
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Ok(()),
            Err(_) => {
                // Deadline passed. Cancel our queue entry; if it is already
                // gone we were granted concurrently — keep the slot.
                let mut inner = self.lock_inner();
                let queue = &mut inner.queues[priority.index()];
                match queue.iter().position(|w| w.id == id) {
                    Some(pos) => {
                        queue.remove(pos);
                        let deadline_msg = format!(
                            "provider concurrency queue saturated; waited {}s",
                            MAX_RATE_WAIT.as_secs()
                        );
                        Err(GatewayError::Provider(deadline_msg))
                    }
                    None => Ok(()),
                }
            }
        }
    }

    /// Return `n` slots to the pool, granting each to the highest-priority
    /// live waiter. A waiter that left (oneshot closed) is skipped and its
    /// slot re-granted to the next in line. Synchronous and panic-free:
    /// only the `Drop` path calls this.
    fn release(&self, n: usize) {
        let mut inner = self.lock_inner();
        inner.available += n;
        for _ in 0..n {
            if inner.available == 0 {
                break;
            }
            let Some(queue_idx) = (0..4).find(|i| !inner.queues[*i].is_empty()) else {
                break;
            };
            let waiter = inner.queues[queue_idx].pop_front().expect("queue is non-empty");
            if waiter.tx.send(()).is_ok() {
                inner.available -= 1;
            }
        }
    }
}

#[derive(Debug)]
struct Bucket {
    gate: Arc<ConcurrencyGate>,
    inner: Mutex<BucketInner>,
}

impl Bucket {
    fn new(cfg: RateConfig) -> Self {
        Bucket {
            gate: Arc::new(ConcurrencyGate::new(cfg.max_concurrency)),
            inner: Mutex::new(BucketInner::new(cfg)),
        }
    }

    /// Wait for a rate token (bounded by `MAX_RATE_WAIT`), fast-failing if a
    /// provider-429 cooldown is armed or the hourly window is full. Does not
    /// hold the bucket lock across the wait so other providers/requests are
    /// unaffected.
    async fn acquire_token(&self, cfg: RateConfig) -> Result<(), GatewayError> {
        let deadline = Instant::now() + MAX_RATE_WAIT;
        loop {
            {
                let mut inner = self.inner.lock().await;
                if let Some(until) = inner.cooldown_until {
                    if until > Instant::now() {
                        let remaining = until.saturating_duration_since(Instant::now());
                        return Err(GatewayError::Provider(format!(
                            "provider is rate limited; retry after {}s",
                            remaining.as_secs()
                        )));
                    }
                    inner.cooldown_until = None;
                }
                inner.refill();
                if cfg.messages_per_hour > 0
                    && inner.hourly_count() >= cfg.messages_per_hour as usize
                {
                    return Err(GatewayError::Provider(format!(
                        "provider hourly message rate limit reached ({} in the last hour); retry later",
                        cfg.messages_per_hour
                    )));
                }
                if inner.tokens >= 1.0 {
                    inner.tokens -= 1.0;
                    inner.hourly.push_back(Instant::now());
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(GatewayError::Provider(format!(
                    "provider rate limit queue saturated; waited {}s",
                    MAX_RATE_WAIT.as_secs()
                )));
            }
            let step = Duration::from_secs_f64(1.0 / (cfg.requests_per_minute as f64 / 60.0));
            let step = if step.is_zero() { Duration::from_millis(1) } else { step };
            tokio::time::sleep(step.min(Duration::from_secs(1))).await;
        }
    }
}

/// Guard holding one concurrency permit; released on drop.
/// Guard holding one concurrency slot; the slot is released back to the
/// provider's wait list when this drops, immediately granting the next
/// highest-priority waiter.
#[derive(Debug)]
pub struct RateLimitPermit {
    gate: Arc<ConcurrencyGate>,
}

impl Drop for RateLimitPermit {
    fn drop(&mut self) {
        self.gate.release(1);
    }
}

/// Per-provider rate limiter: token bucket for sustained rate + priority
/// concurrency gate + rolling hourly cap + fast-fail cooldown after 429s.
#[derive(Clone, Default)]
pub struct ProviderRateLimiter {
    inner: Arc<Mutex<HashMap<String, Arc<Bucket>>>>,
}

impl ProviderRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    async fn bucket(&self, provider: &str) -> Arc<Bucket> {
        let mut map = self.inner.lock().await;
        map.entry(provider.to_string())
            .or_insert_with(|| Arc::new(Bucket::new(rate_config_for(provider))))
            .clone()
    }

    /// Acquire rate + concurrency authorization for one request. Waits up to
    /// `MAX_RATE_WAIT` for a rate token, then queues for a concurrency slot
    /// at `priority` (higher priority moves the queue ahead). The returned
    /// permit releases the slot on drop.
    pub async fn acquire(
        &self,
        provider: &str,
        priority: Priority,
    ) -> Result<RateLimitPermit, GatewayError> {
        let cfg = rate_config_for(provider);
        let bucket = self.bucket(provider).await;
        bucket.acquire_token(cfg).await?;
        let deadline = Instant::now() + MAX_RATE_WAIT;
        bucket.gate.acquire(priority, deadline).await?;
        Ok(RateLimitPermit {
            gate: bucket.gate.clone(),
        })
    }

    /// Record a provider 429 with an optional `Retry-After` hint. Requests
    /// fast-fail while the cooldown is armed.
    pub async fn record_ratelimited(&self, provider: &str, retry_after: Option<Duration>) {
        let bucket = self.bucket(provider).await;
        let cfg = rate_config_for(provider);
        let mut inner = bucket.inner.lock().await;
        inner.cooldown_until = Some(Instant::now() + retry_after.unwrap_or(cfg.cooldown_after_429));
    }

    /// Clear rate-limit state (e.g. after the provider healed).
    #[cfg(test)]
    pub async fn reset(&self, provider: &str) {
        let bucket = self.bucket(provider).await;
        let mut inner = bucket.inner.lock().await;
        inner.cooldown_until = None;
        inner.tokens = inner.capacity;
    }
}

/// Classify a failed provider request into an [`ErrorClass`].
///
/// Uses the provider name plus whatever the adapter surfaced: an optional
/// HTTP status and the human message. Marker lookup is intentionally a flat
/// table keyed by provider so a marker from one provider never misfires on
/// another.
pub fn classify_error(provider: &str, status: Option<u16>, message: &str) -> ErrorClass {
    let msg = message.to_lowercase();

    // Authorization failures that must read as "missing" rather than "expired".
    if matches!(
        provider,
        "claude" | "glm" | "grok" | "kimi" | "metaai" | "mimo" | "mistral"
    ) && looks_like_missing_credentials(provider, &msg)
    {
        return ErrorClass::AuthMissing;
    }

    if matches!(status, Some(401)) {
        return ErrorClass::AuthExpired;
    }

    // Challenge gates beat generic 403 handling: grok anti-bot, GLM captcha
    // gates, and cloudflare challenge markers all mean "verify, then retry".
    if matches!(status, Some(403)) && provider == "grok" {
        return ErrorClass::Challenge;
    }
    if contains_any(
        &msg,
        &[
            "captcha",
            "人机验证",
            "antibot",
            "anti-bot",
            "illegalusertag",
            "challenge",
            "verify you are human",
            "page was deleted",
        ],
    ) {
        return ErrorClass::Challenge;
    }

    if matches!(status, Some(429)) {
        return ErrorClass::RateLimit;
    }
    if contains_any(
        &msg,
        &[
            "6200",
            "rate limit",
            "message rate limit",
            "too many requests",
            "request rate limit",
        ],
    ) {
        return ErrorClass::RateLimit;
    }

    if contains_any(
        &msg,
        &[
            "42212",
            "2056",
            "token plan",
            "plan usage",
            "quota",
            "credit",
            "insufficient balance",
            "usage cap",
            "account locked",
        ],
    ) {
        return ErrorClass::Quota;
    }

    if contains_any(&msg, &["token_expired", "session expired", "auth session"]) {
        return ErrorClass::AuthExpired;
    }

    if contains_any(
        &msg,
        &[
            "decode failed",
            "empty response body",
            "unexpected sse",
            "no content",
            "invalid json",
            "unknown phase",
        ],
    ) {
        return ErrorClass::Parse;
    }

    if contains_any(
        &msg,
        &["timed out", "timeout after", "connection reset", "connection refused", "dns"],
    ) {
        return ErrorClass::Network;
    }

    if contains_any(
        &msg,
        &["model .* not found", "unknown model", "invalid model", "model does not exist"],
    ) {
        return ErrorClass::ModelUnsupported;
    }

    if contains_any(
        &msg,
        &[
            "protocol",
            "drift",
            "no longer",
            "unexpected shape",
            "marker",
            "missing field",
            "unexpected field",
        ],
    ) {
        return ErrorClass::Drift;
    }

    ErrorClass::Other
}

fn looks_like_missing_credentials(provider: &str, msg: &str) -> bool {
    let markers: &[&str] = match provider {
        "claude" => &["sessionkey", "lastactiveorg"],
        "glm" => &["token", "local storage"],
        "grok" => &["sso", "missing cookie", "not logged in"],
        "kimi" => &["missing cookie", "not logged in"],
        "metaai" => &["session", "not logged in"],
        "mimo" => &["xiaomichatbot", "userid", "missing cookie"],
        "mistral" => &["missing session", "session cookie"],
        _ => &[],
    };
    contains_any(msg, markers) && contains_any(msg, &["missing", "not found", "not logged in", "absent"])
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let haystack = haystack.to_lowercase();
    needles.iter().any(|n| haystack.contains(n))
}

/// Stream wrapper that records provider health on termination: success when
/// the stream completes cleanly *and* produced at least one meaningful
/// content delta, a classified error when the first error item appears or
/// the stream is dropped early. A stream that ends without ever producing
/// content is treated as a failure, so empty responses cannot mask a broken
/// parse or a dead provider connection.
pub struct HealthTrackingStream {
    inner: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send + 'static>>,
    health: ProviderHealthRegistry,
    provider: String,
    settled: bool,
    saw_delta: bool,
}

impl HealthTrackingStream {
    pub fn wrap(
        inner: BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
        health: ProviderHealthRegistry,
        provider: String,
    ) -> Self {
        HealthTrackingStream {
            inner,
            health,
            provider,
            settled: false,
            saw_delta: false,
        }
    }
}

/// A chunk is "meaningful" if it carries content, reasoning, citations, or
/// tool calls. A bare role marker or an empty frame does not count, so a
/// stream that only emitted framing is not mistaken for a real answer.
fn chunk_has_meaningful_delta(chunk: &ChatCompletionChunk) -> bool {
    chunk.choices.iter().any(|choice| {
        let d = &choice.delta;
        d.content.as_deref().is_some_and(|c| !c.is_empty())
            || d.reasoning_content.as_deref().is_some_and(|c| !c.is_empty())
            || d.citations.as_ref().is_some_and(|c| !c.is_empty())
            || d.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
    })
}

impl Stream for HealthTrackingStream {
    type Item = Result<ChatCompletionChunk, GatewayError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(item))) => {
                if chunk_has_meaningful_delta(&item) {
                    this.saw_delta = true;
                }
                Poll::Ready(Some(Ok(item)))
            }
            Poll::Ready(Some(Err(e))) => {
                if !this.settled {
                    this.settled = true;
                    let class = classify_error(&this.provider, None, &e.to_string());
                    spawn_health_record(this.health.clone(), this.provider.clone(), class);
                }
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                if !this.settled {
                    this.settled = true;
                    if this.saw_delta {
                        spawn_health_success(this.health.clone(), this.provider.clone());
                    } else {
                        spawn_health_record(
                            this.health.clone(),
                            this.provider.clone(),
                            ErrorClass::Other,
                        );
                    }
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn spawn_health_record(health: ProviderHealthRegistry, provider: String, class: ErrorClass) {
    tokio::spawn(async move {
        health.record_error(&provider, class).await;
    });
}

fn spawn_health_success(health: ProviderHealthRegistry, provider: String) {
    tokio::spawn(async move {
        health.record_success(&provider).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChatMessageDelta, ChunkChoice};
    use futures::StreamExt;

    #[tokio::test]
    async fn healthy_provider_is_not_gated() {
        let health = ProviderHealthRegistry::new();
        assert!(health.gate_new_request("glm").await.is_ok());
    }

    #[tokio::test]
    async fn authexpired_opens_circuit_until_cooldown_then_half_open() {
        let health = ProviderHealthRegistry::new();
        health.record_error("glm", ErrorClass::AuthExpired).await;
        let err = health.gate_new_request("glm").await.unwrap_err();
        assert!(err.public_message().contains("auth_expired"));

        // Canary probe after the (long) cooldown heals the circuit.
        {
            let mut map = health.inner.lock().await;
            let s = map.get_mut("glm").unwrap();
            s.lockout_until = Some(Instant::now() - Duration::from_secs(1));
        }
        assert!(health.gate_new_request("glm").await.is_ok());
        assert_eq!(health.status("glm").await.state, HealthState::Degraded);
        health.record_success("glm").await;
        assert_eq!(health.status("glm").await.state, HealthState::Healthy);
    }

    #[tokio::test]
    async fn parse_failures_do_not_open_the_circuit_before_third() {
        let health = ProviderHealthRegistry::new();
        // First two parse failures degrade but keep accepting traffic.
        health.record_error("grok", ErrorClass::Parse).await;
        health.record_error("grok", ErrorClass::Parse).await;
        assert!(health.gate_new_request("grok").await.is_ok());
        assert_eq!(health.status("grok").await.state, HealthState::Degraded);
        // The third consecutive parse failure opens the circuit: repeated
        // unparseable responses are upstream drift in disguise.
        health.record_error("grok", ErrorClass::Parse).await;
        let err = health.gate_new_request("grok").await.unwrap_err();
        assert!(err.public_message().contains("degraded"));
    }

    #[tokio::test]
    async fn parse_success_resets_degradation() {
        let health = ProviderHealthRegistry::new();
        health.record_error("glm", ErrorClass::Parse).await;
        health.record_error("glm", ErrorClass::Parse).await;
        health.record_success("glm").await;
        assert_eq!(health.status("glm").await.state, HealthState::Healthy);
        // A lone parse failure after recovery still does not open.
        health.record_error("glm", ErrorClass::Parse).await;
        assert!(health.gate_new_request("glm").await.is_ok());
    }

    #[tokio::test]
    async fn snapshot_reports_state_and_lockout() {
        let health = ProviderHealthRegistry::new();
        health.record_error("grok", ErrorClass::Challenge).await;
        let snaps = health.snapshot().await;
        let grok = snaps.iter().find(|s| s.provider == "grok").unwrap();
        assert_eq!(grok.state, HealthState::ChallengeRequired);
        assert!(grok.locked_out_secs > 0);
        assert_eq!(grok.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn rate_limiter_allows_burst_then_refills() {
        let limiter = ProviderRateLimiter::new();
        // Warm the bucket into the map with a custom-ish provider so the
        // default config applies; take `burst` tokens without waiting.
        for _ in 0..rate_config_for("kimi").burst {
            assert!(limiter.acquire("kimi", Priority::UserLive).await.is_ok());
        }
        // Continued acquisition must not fail hard, just wait for a refill.
        let _ = limiter.acquire("kimi", Priority::UserLive).await;
    }

    #[tokio::test]
    async fn rate_limiter_429_cooldown_fails_fast() {
        let limiter = ProviderRateLimiter::new();
        limiter
            .record_ratelimited("mistral", Some(Duration::from_secs(30)))
            .await;
        let err = limiter.acquire("mistral", Priority::UserLive).await.unwrap_err();
        assert!(err.public_message().contains("rate limited"));
        limiter.reset("mistral").await;
        assert!(limiter.acquire("mistral", Priority::UserLive).await.is_ok());
    }

    #[tokio::test]
    async fn rate_limiter_429_cooldown_expires_on_its_own() {
        let limiter = ProviderRateLimiter::new();
        limiter
            .record_ratelimited("kimi", Some(Duration::from_millis(150)))
            .await;
        assert!(limiter.acquire("kimi", Priority::UserLive).await.is_err());
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(limiter.acquire("kimi", Priority::UserLive).await.is_ok());
    }

    #[tokio::test]
    async fn hourly_cap_rejects_once_window_is_full() {
        let bucket = Bucket::new(RateConfig::default());
        let capped = RateConfig {
            messages_per_hour: 2,
            ..RateConfig::default()
        };
        assert!(bucket.acquire_token(capped).await.is_ok());
        assert!(bucket.acquire_token(capped).await.is_ok());
        let err = bucket.acquire_token(capped).await.unwrap_err();
        assert!(err.public_message().contains("hourly"));
    }

    #[tokio::test]
    async fn concurrency_gate_prefers_live_users_over_canaries() {
        let gate = Arc::new(ConcurrencyGate::new(1));
        let deadline = Instant::now() + Duration::from_secs(10);
        let held = gate.acquire(Priority::UserLive, deadline).await;
        assert!(held.is_ok());
        let g = gate.clone();
        let mut canary = tokio::spawn(async move { g.acquire(Priority::Canary, deadline).await });
        let g = gate.clone();
        let live = tokio::spawn(async move { g.acquire(Priority::UserLive, deadline).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.release(1);
        let live_granted = tokio::time::timeout(Duration::from_secs(5), live)
            .await
            .expect("live waiter must be granted")
            .unwrap();
        live_granted.unwrap();
        let canary_result = tokio::time::timeout(Duration::from_millis(200), &mut canary).await;
        assert!(canary_result.is_err(), "canary must not complete before a slot frees");
        gate.release(1);
        let canary_granted = tokio::time::timeout(Duration::from_secs(5), canary)
            .await
            .expect("canary must be granted once a slot frees")
            .unwrap();
        canary_granted.unwrap();
    }

    #[tokio::test]
    async fn concurrency_gate_is_fifo_within_a_priority() {
        let gate = Arc::new(ConcurrencyGate::new(1));
        let deadline = Instant::now() + Duration::from_secs(10);
        assert!(
            gate.acquire(Priority::Canary, deadline).await.is_ok(),
            "slot must be held"
        );
        let g = gate.clone();
        let first = tokio::spawn(async move { g.acquire(Priority::Canary, deadline).await });
        let g = gate.clone();
        let mut second = tokio::spawn(async move { g.acquire(Priority::Canary, deadline).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.release(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), first)
                .await
                .expect("first queued waiter must be granted")
                .unwrap()
                .is_ok()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut second).await.is_err(),
            "second waiter must wait for its turn"
        );
        gate.release(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), &mut second)
                .await
                .expect("second waiter must be granted in order")
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn concurrency_gate_saturated_wait_times_out() {
        let gate = ConcurrencyGate::new(1);
        let deadline = Instant::now() + Duration::from_secs(10);
        assert!(gate.acquire(Priority::UserLive, deadline).await.is_ok());
        let err = gate
            .acquire(Priority::UserLive, Instant::now())
            .await
            .unwrap_err();
        assert!(err.public_message().contains("saturated"));
    }

    fn chunk_with(content: Option<&str>, tool_calls: bool) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "chunk-1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "test".to_string(),
            session_url: None,
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChatMessageDelta {
                    role: Some("assistant".to_string()),
                    content: content.map(|c| c.to_string()),
                    reasoning_content: None,
                    citations: None,
                    tool_calls: if tool_calls { Some(vec![]) } else { None },
                },
                finish_reason: None,
            }],
        }
    }

    #[tokio::test]
    async fn empty_stream_without_delta_records_failure() {
        let health = ProviderHealthRegistry::new();
        let stream = futures::stream::iter(Vec::<Result<ChatCompletionChunk, GatewayError>>::new());
        let tracked = HealthTrackingStream::wrap(
            Box::pin(stream),
            health.clone(),
            "stream-probe".to_string(),
        );
        let collected: Vec<_> = tracked.collect().await;
        assert!(collected.is_empty());
        tokio::time::sleep(Duration::from_millis(50)).await;
        let snap = health.snapshot().await;
        let probe = snap.iter().find(|s| s.provider == "stream-probe").unwrap();
        assert_eq!(probe.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn stream_with_meaningful_delta_records_success() {
        let health = ProviderHealthRegistry::new();
        let chunks = vec![
            Ok(chunk_with(Some("hello"), false)),
            Ok(chunk_with(None, true)),
        ];
        let stream = futures::stream::iter(chunks);
        let tracked = HealthTrackingStream::wrap(
            Box::pin(stream),
            health.clone(),
            "stream-probe".to_string(),
        );
        let collected: Vec<_> = tracked.collect().await;
        assert_eq!(collected.len(), 2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let snap = health.snapshot().await;
        let probe = snap.iter().find(|s| s.provider == "stream-probe").unwrap();
        assert_eq!(probe.consecutive_failures, 0);
        assert_eq!(probe.state, HealthState::Healthy);
    }

    #[test]
    fn classify_statuses_are_authoritative() {
        assert_eq!(classify_error("grok", Some(401), "unauthorized"), ErrorClass::AuthExpired);
        assert_eq!(classify_error("grok", Some(403), "forbidden"), ErrorClass::Challenge);
        assert_eq!(classify_error("glm", Some(429), "rate limited"), ErrorClass::RateLimit);
        assert_eq!(classify_error("kimi", None, "retry later"), ErrorClass::Other);
    }

    #[test]
    fn classify_provider_markers() {
        assert_eq!(
            classify_error("glm", None, "FrontendCaptchaRequired, 人机验证"),
            ErrorClass::Challenge
        );
        assert_eq!(
            classify_error("glm", None, "IllegalUserTag, please verify"),
            ErrorClass::Challenge
        );
        assert_eq!(
            classify_error("glm", None, "42212 使用量达到上限"),
            ErrorClass::Quota
        );
        assert_eq!(
            classify_error("mistral", None, "response decode failed"),
            ErrorClass::Parse
        );
        assert_eq!(
            classify_error("mistral", None, "message rate limit 6200"),
            ErrorClass::RateLimit
        );
        assert_eq!(
            classify_error("grok", None, "session expired"),
            ErrorClass::AuthExpired
        );
        assert_eq!(
            classify_error("claude", None, "sessionKey missing not found"),
            ErrorClass::AuthMissing
        );
        assert_eq!(
            classify_error("mimo", None, "xiaomichatbot_ph cookie not found"),
            ErrorClass::AuthMissing
        );
    }
}