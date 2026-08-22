use std::sync::Arc;

use crate::providers::authcheck::CachedAuthChecker;
use crate::providers::health::{ProviderHealthRegistry, ProviderRateLimiter};
use crate::{config::Config, providers::ProviderRegistry, session::SessionManager};

/// Application state shared by all request handlers.
///
/// Holds immutable configuration, the provider registry, the browser session
/// manager, and the infra guards (circuit breaker, rate limiter, auth
/// pre-flight cache). Cloning is cheap because the registry, manager, and
/// guards are all cheap `Arc`/`Clone` handles.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub providers: Arc<ProviderRegistry>,
    pub sessions: SessionManager,
    pub health: ProviderHealthRegistry,
    pub rate_limiter: ProviderRateLimiter,
    pub auth_checker: CachedAuthChecker,
}

impl AppState {
    pub fn new(config: Config, providers: ProviderRegistry, sessions: SessionManager) -> Self {
        Self {
            config,
            providers: Arc::new(providers),
            sessions,
            health: ProviderHealthRegistry::new(),
            rate_limiter: ProviderRateLimiter::new(),
            auth_checker: CachedAuthChecker::new(),
        }
    }
}
