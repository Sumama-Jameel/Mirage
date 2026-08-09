use std::collections::HashMap;
use std::sync::Arc;

use crate::error::GatewayError;

/// HTTP header produced by solving a PoW challenge.
///
/// The provider attaches this header to its API request to pass
/// the PoW gate.
#[derive(Debug, Clone)]
pub struct PoWHeader {
    /// HTTP header name, e.g. `"x-ds-pow-response"`.
    pub name: String,
    /// HTTP header value, the solved challenge payload.
    pub value: String,
}

/// A compiled PoW solver for a specific provider's challenge format.
///
/// Each implementation encapsulates:
/// - The compiled WASM module (or other solving strategy)
/// - Knowledge of the challenge schema
/// - How to format the response header
pub trait PoWSolver: Send + Sync {
    /// Unique identifier for this solver (e.g. `"deepseek-v1"`).
    fn name(&self) -> &'static str;

    /// Given a raw challenge response JSON from a provider's PoW
    /// challenge endpoint, produce the HTTP header to attach to the
    /// subsequent API request.
    fn solve(&self, challenge: &serde_json::Value) -> Result<PoWHeader, GatewayError>;
}

/// Ordered list of solver names for fallback.
///
/// Providers declare their fallback chain at construction time:
/// `["deepseek-v1", "generic-pow", ...]`. [`SolverRegistry::solve_fallback`]
/// tries each in order and returns the first success.
pub type SolverChain = Vec<String>;

/// Registry of named [`PoWSolver`] instances.
///
/// Cheaply cloneable (`Arc` inside). Populated at startup and shared
/// across all providers.
#[derive(Clone, Default)]
pub struct SolverRegistry {
    solvers: HashMap<String, Arc<dyn PoWSolver>>,
}

impl SolverRegistry {
    pub fn new() -> Self {
        Self {
            solvers: HashMap::new(),
        }
    }

    /// Register a solver under [`PoWSolver::name`].
    ///
    /// If a solver with the same name already exists it is replaced.
    pub fn register(&mut self, solver: Arc<dyn PoWSolver>) {
        self.solvers.insert(solver.name().to_string(), solver);
    }

    /// Look up a solver by name.
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<Arc<dyn PoWSolver>> {
        self.solvers.get(name).cloned()
    }

    /// Try each solver in `chain` in order; return the first success.
    ///
    /// If a solver is not found or returns an error, the next solver
    /// is tried. When all fail the *last* error is returned.
    ///
    /// Returns `GatewayError::Internal("no PoW solvers configured")`
    /// when `chain` is empty.
    pub fn solve_fallback(
        &self,
        chain: &SolverChain,
        challenge: &serde_json::Value,
    ) -> Result<PoWHeader, GatewayError> {
        let mut last_err: Option<GatewayError> = None;

        for name in chain {
            let solver = match self.solvers.get(name) {
                Some(s) => s,
                None => {
                    tracing::warn!(solver = %name, "PoW solver not registered; skipping");
                    last_err = Some(GatewayError::Internal(format!(
                        "PoW solver '{name}' is not registered"
                    )));
                    continue;
                }
            };

            match solver.solve(challenge) {
                Ok(header) => {
                    tracing::debug!(solver = %name, "PoW challenge solved");
                    return Ok(header);
                }
                Err(e) => {
                    tracing::warn!(solver = %name, error = %e, "PoW solver failed; trying next");
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            GatewayError::Internal("no PoW solvers configured in chain".to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummySolver;

    impl PoWSolver for DummySolver {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn solve(&self, _challenge: &serde_json::Value) -> Result<PoWHeader, GatewayError> {
            Ok(PoWHeader {
                name: "x-pow".to_string(),
                value: "solved".to_string(),
            })
        }
    }

    struct FailingSolver;

    impl PoWSolver for FailingSolver {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn solve(&self, _challenge: &serde_json::Value) -> Result<PoWHeader, GatewayError> {
            Err(GatewayError::Provider("solver failed".to_string()))
        }
    }

    #[test]
    fn register_and_retrieve() {
        let mut reg = SolverRegistry::new();
        reg.register(Arc::new(DummySolver));
        assert!(reg.get("dummy").is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn fallback_uses_first_success() {
        let mut reg = SolverRegistry::new();
        reg.register(Arc::new(FailingSolver));
        reg.register(Arc::new(DummySolver));

        let chain = vec!["failing".to_string(), "dummy".to_string()];
        let result = reg.solve_fallback(&chain, &serde_json::json!({}));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, "solved");
    }

    #[test]
    fn fallback_returns_last_error_when_all_fail() {
        let mut reg = SolverRegistry::new();
        reg.register(Arc::new(FailingSolver));

        let chain = vec!["failing".to_string()];
        let result = reg.solve_fallback(&chain, &serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn fallback_skips_missing_solvers() {
        let mut reg = SolverRegistry::new();
        reg.register(Arc::new(DummySolver));

        let chain = vec!["missing".to_string(), "dummy".to_string()];
        let result = reg.solve_fallback(&chain, &serde_json::json!({}));
        assert!(result.is_ok());
    }

    #[test]
    fn empty_chain_returns_error() {
        let reg = SolverRegistry::new();
        let empty: SolverChain = vec![];
        let result = reg.solve_fallback(&empty, &serde_json::json!({}));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no PoW solvers configured"));
    }
}
