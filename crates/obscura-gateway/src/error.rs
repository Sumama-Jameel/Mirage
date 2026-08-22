use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Gateway-wide error type.
///
/// Every error carries a user-safe message and maps to an appropriate HTTP
/// status code. Internal details are logged but never returned to clients.
#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("authentication error: {0}")]
    Auth(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("provider error: {0}")]
    Provider(String),

    /// Provider answered 429. `retry_after` carries the provider's
    /// `Retry-After` hint (or the gateway's own cooldown) so the client and
    /// the local rate limiter can both back off.
    #[error("provider is rate limited: {message}")]
    ProviderRateLimited {
        message: String,
        retry_after: Option<std::time::Duration>,
    },

    #[error("internal error: {0}")]
    Internal(String),
}

impl GatewayError {
    /// Public-facing error message. Never includes secrets or stack traces.
    pub fn public_message(&self) -> String {
        self.to_string()
    }

    /// How long to wait before retrying, if the provider told us (or the
    /// cooldown knows).
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            GatewayError::ProviderRateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    pub(crate) fn status_code(&self) -> StatusCode {
        match self {
            GatewayError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            GatewayError::Auth(_) => StatusCode::UNAUTHORIZED,
            GatewayError::BadRequest(_) => StatusCode::BAD_REQUEST,
            GatewayError::Provider(_) => StatusCode::BAD_GATEWAY,
            GatewayError::ProviderRateLimited { .. } => StatusCode::BAD_GATEWAY,
            GatewayError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = Json(json!({
            "error": {
                "message": self.public_message(),
                "type": "gateway_error",
            }
        }));
        let mut response = (status, body).into_response();
        if let Some(retry_after) = self.retry_after() {
            if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after.as_secs().to_string())
            {
                response
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
            }
        }
        response
    }
}

impl From<config::ConfigError> for GatewayError {
    fn from(err: config::ConfigError) -> Self {
        GatewayError::Config(err.to_string())
    }
}

impl From<std::io::Error> for GatewayError {
    fn from(err: std::io::Error) -> Self {
        GatewayError::Internal(err.to_string())
    }
}

impl From<serde_json::Error> for GatewayError {
    fn from(err: serde_json::Error) -> Self {
        GatewayError::BadRequest(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_maps_to_internal_status() {
        let err = GatewayError::Config("bad port".to_string());
        assert!(matches!(err.status_code(), StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[test]
    fn auth_error_maps_to_unauthorized() {
        let err = GatewayError::Auth("missing token".to_string());
        assert!(matches!(err.status_code(), StatusCode::UNAUTHORIZED));
    }
}
