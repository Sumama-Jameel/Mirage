use axum::{
    extract::{Request, State},
    http::header::AUTHORIZATION,
    middleware::Next,
    response::Response,
};

use crate::{error::GatewayError, state::AppState};

/// Bearer-token authentication middleware.
///
/// Expects `Authorization: Bearer <api_key>`. The API key is configured in
/// `auth.api_key` and defaults to `obscura-local` for local development.
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, GatewayError> {
    let token = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(|| GatewayError::Auth("missing or invalid Authorization header".to_string()))?;

    if token != state.config.auth.api_key {
        return Err(GatewayError::Auth("invalid API key".to_string()));
    }

    Ok(next.run(request).await)
}
