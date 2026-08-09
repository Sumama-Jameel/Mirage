use std::sync::Arc;

use crate::{config::Config, providers::ProviderRegistry, session::SessionManager};

/// Application state shared by all request handlers.
///
/// Holds immutable configuration, the provider registry, and the browser
/// session manager. Cloning is cheap because the registry and manager are
/// wrapped in `Arc`/`Clone` handles.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub providers: Arc<ProviderRegistry>,
    pub sessions: SessionManager,
}

impl AppState {
    pub fn new(config: Config, providers: ProviderRegistry, sessions: SessionManager) -> Self {
        Self {
            config,
            providers: Arc::new(providers),
            sessions,
        }
    }
}
