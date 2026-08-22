//! DeepSeek provider.
//!
//! DeepSeek's web UI is heavily protected (Turnstile, broken Worker polyfill),
//! so this provider bypasses the UI entirely and calls DeepSeek's internal
//! chat API directly. The warmed browser still provides the authenticated
//! session: cookies and the Firefox-imported `userToken` bearer token.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use futures::stream::StreamExt;

use crate::error::GatewayError;
use crate::models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model};
use crate::providers::session_guard::SessionGuardStream;
use crate::providers::Provider;
use crate::providers::solver::{SolverChain, SolverRegistry};
use crate::session::SessionManager;
use crate::state::AppState;

use direct::DirectClient;
use state::SessionStore;

mod direct;
mod pow;
mod pow_assets;

pub use pow::DeepSeekSolver;
pub use pow_assets::PowAssets;
mod state;
mod url;
mod upload;

/// DeepSeek adapter. Cheaply cloneable.
///
/// The PoW solver is held in a shared [`SolverRegistry`] and looked up
/// at request time via the configured [`SolverChain`]. Multiple solvers
/// can be registered and tried in order, making the provider resilient
/// to upstream PoW format changes.
#[derive(Clone)]
pub struct DeepSeekProvider {
    solvers: SolverRegistry,
    solver_chain: SolverChain,
    sessions: SessionStore,
}

impl DeepSeekProvider {
    /// Create a new DeepSeek provider.
    ///
    /// `solvers` must contain at least one solver matching an entry in
    /// `solver_chain` (e.g. `"deepseek-v1"`), otherwise every request
    /// will fail with a "no PoW solvers configured" error.
    #[cfg(test)]
    pub fn new(solvers: SolverRegistry, solver_chain: SolverChain) -> Self {
        Self {
            solvers,
            solver_chain,
            sessions: SessionStore::new(),
        }
    }

    /// Create a provider with optional disk-persisted sessions.
    pub fn with_data_dir(
        solvers: SolverRegistry,
        solver_chain: SolverChain,
        data_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            solvers,
            solver_chain,
            sessions: SessionStore::with_data_dir(data_dir),
        }
    }
}

impl Provider for DeepSeekProvider {
    fn name(&self) -> &'static str {
        "deepseek"
    }

    fn url(&self) -> &'static str {
        "https://chat.deepseek.com"
    }

    fn models(&self) -> Vec<Model> {
        vec![
            Model {
                id: "deepseek-chat".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
            Model {
                id: "deepseek-instant".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
            Model {
                id: "deepseek-reasoner".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
            Model {
                id: "deepseek-expert".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
            Model {
                id: "deepseek-vision".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
            // V4 / R1 aliases from docs/LatestAImodels, routed to the same
            // verified wire types (v4-pro/r1 -> expert, v4-flash/v3.2 -> default).
            Model {
                id: "deepseek-v4-pro".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
            Model {
                id: "deepseek-v4-flash".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
            Model {
                id: "deepseek-v3.2".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
            Model {
                id: "deepseek-r1".to_string(),
                object: "model".to_string(),
                created: 1_700_000_000,
                owned_by: "deepseek".to_string(),
            },
        ]
    }


    fn chat(
        &self,
        sessions: &SessionManager,
        _state: &AppState,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>> {
        let this = self.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            let session = sessions.acquire().await?;

            let client = match DirectClient::new(
                session.clone(),
                this.solvers.clone(),
                this.solver_chain.clone(),
                &request.model,
                this.sessions.clone(),
            ) {
                Ok(c) => c,
                Err(e) => {
                    let _ = sessions.release(session.id, false).await;
                    return Err(e);
                }
            };

            let result = client.chat(request).await;
            let _ = sessions.release(session.id.clone(), false).await;
            result
        })
    }

    fn chat_stream(
        &self,
        sessions: &SessionManager,
        _state: &AppState,
        request: ChatCompletionRequest,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        futures::stream::BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
                        GatewayError,
                    >,
                > + Send,
        >,
    > {
        let this = self.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            let session = sessions.acquire().await?;

            let client = match DirectClient::new(
                session.clone(),
                this.solvers.clone(),
                this.solver_chain.clone(),
                &request.model,
                this.sessions.clone(),
            ) {
                Ok(c) => c,
                Err(e) => {
                    let _ = sessions.release(session.id, false).await;
                    return Err(e);
                }
            };

            // The stream itself owns the session release guard so the session is
            // returned only after the consumer drops the stream.
            let session_id = session.id.clone();
            let sessions_for_stream = sessions.clone();
            let stream = client.chat_stream(request).await?;
            let guarded = SessionGuardStream::new(stream, sessions_for_stream, session_id);
            Ok(guarded.boxed())
        })
    }





    fn supports_attachments(&self) -> bool {
        true
    }

    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), GatewayError> {
        // DeepSeek web API supports native JSON mode for all non-vision models.
        // Verified: response_format is accepted by the internal /api/v0/chat/completion endpoint.
        if let Some(fmt) = &request.response_format {
            if fmt.r#type == "json_object" {
                // Vision model does not support JSON mode constraints
                if request.model == "deepseek-vision" {
                    return Err(GatewayError::BadRequest(format!(
                        "DeepSeek model '{}' does not support response_format \"json_object\"",
                        request.model
                    )));
                }
            }
        }

        let supports_thinking = matches!(
            request.model.as_str(),
            "deepseek-reasoner" | "deepseek-expert" | "deepseek-v4-pro" | "deepseek-r1"
        );
        if request.thinking == Some(true) && !supports_thinking {
            return Err(GatewayError::BadRequest(format!(
                "DeepSeek model '{}' does not support thinking (use deepseek-reasoner, deepseek-expert, deepseek-v4-pro, or deepseek-r1)",
                request.model
            )));
        }
        let has_images = request.messages.iter().any(|m| !m.content.image_urls().is_empty());
        let supports_vision = request.model == "deepseek-vision";
        if has_images && !supports_vision {
            return Err(GatewayError::BadRequest(format!(
                "DeepSeek model '{}' does not support image inputs (use deepseek-vision or let auto-detection choose it)",
                request.model
            )));
        }
        Ok(())
    }

}

/// A stream wrapper that releases the browser session when the consumer is
/// done, even if the underlying SSE stream ends with an error.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_provider_exposes_expected_config() {
        let p = DeepSeekProvider::new(SolverRegistry::new(), vec!["deepseek-v1".to_string()]);
        assert_eq!(p.name(), "deepseek");
        assert_eq!(p.url(), "https://chat.deepseek.com");
        assert_eq!(p.models().len(), 9);
        assert!(p.models().iter().any(|m| m.id == "deepseek-chat"));
        assert!(p.models().iter().any(|m| m.id == "deepseek-instant"));
        assert!(p.models().iter().any(|m| m.id == "deepseek-reasoner"));
        assert!(p.models().iter().any(|m| m.id == "deepseek-expert"));
        assert!(p.models().iter().any(|m| m.id == "deepseek-vision"));
        assert!(p.models().iter().any(|m| m.id == "deepseek-v4-pro"));
        assert!(p.models().iter().any(|m| m.id == "deepseek-v4-flash"));
        assert!(p.models().iter().any(|m| m.id == "deepseek-v3.2"));
        assert!(p.models().iter().any(|m| m.id == "deepseek-r1"));

    }
}
