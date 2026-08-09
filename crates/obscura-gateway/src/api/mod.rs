use axum::{
    extract::State,
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::stream::{self, BoxStream, StreamExt};
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::info;

use crate::{
    error::GatewayError,
    models::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelsResponse,
    },
    state::AppState,
};

mod auth;

/// Build the gateway HTTP router.
///
/// Routes:
/// - `GET /health` — unauthenticated health check
/// - `GET /v1/models` — OpenAI-compatible model list
/// - `POST /v1/chat/completions` — OpenAI-compatible chat completion
///
/// All `/v1/*` routes require a valid `Authorization: Bearer <api_key>` header.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));

    Router::new()
        .route("/health", get(health_check))
        .merge(protected)
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health_check() -> &'static str {
    "ok"
}

async fn list_models(State(state): State<AppState>) -> Result<Json<ModelsResponse>, GatewayError> {
    Ok(Json(ModelsResponse {
        object: "list".to_string(),
        data: state.providers.all_models(),
    }))
}

async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Response, GatewayError> {
    req.validate()?;

    info!(model = %req.model, stream = req.stream, "chat completion request");

    let provider = state.providers.get(&req.model).ok_or_else(|| {
        GatewayError::BadRequest(format!("model '{}' is not available", req.model))
    })?;
    provider.validate_request(&req)?;

    if req.stream {
        let chunk_stream: BoxStream<
            'static,
            Result<ChatCompletionChunk, GatewayError>,
        > = provider.chat_stream(&state.sessions, &state, req).await?;
        let sse_stream = chunk_stream.map(|result| {
            result.and_then(|chunk| {
                serde_json::to_string(&chunk)
                    .map_err(|e| GatewayError::Internal(format!("SSE serialization failed: {e}")))
            })
        });
        let done_stream = stream::once(async {
            Ok::<_, GatewayError>("[DONE]".to_string())
        });
        let events = sse_stream
            .chain(done_stream)
            .map(|result: Result<String, GatewayError>| {
                result.map(|data| axum::response::sse::Event::default().data(data))
            });
        Ok(Sse::new(events).into_response())
    } else {
        let response: ChatCompletionResponse = provider.chat(&state.sessions, &state, req).await?;
        Ok(Json(response).into_response())
    }
}



#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use futures::stream::BoxStream;
    use std::future::Future;
    use std::pin::Pin;
    use tower::ServiceExt;

    use crate::{
        config::Config,
        error::GatewayError,
        models::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Model},
        providers::{DoneSignal, Provider, ProviderRegistry},
        session::SessionManager,
        state::AppState,
    };

    use super::router;

    /// A minimal provider used by API routing/auth tests. No real chat
    /// execution is exercised here; the tests only verify the HTTP surface.
    #[derive(Clone)]
    struct TestProvider;

    impl Provider for TestProvider {
        fn name(&self) -> &'static str {
            "test"
        }

        fn url(&self) -> &'static str {
            "https://example.com"
        }

        fn models(&self) -> Vec<Model> {
            vec![Model {
                id: "test-model".to_string(),
                object: "model".to_string(),
                created: 1,
                owned_by: "test".to_string(),
            }]
        }

        fn chat(
            &self,
            _sessions: &SessionManager,
            _state: &AppState,
            _request: ChatCompletionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send>> {
            Box::pin(async {
                Err(GatewayError::Internal(
                    "TestProvider does not support chat".to_string(),
                ))
            })
        }

        fn chat_stream(
            &self,
            _sessions: &SessionManager,
            _state: &AppState,
            _request: ChatCompletionRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
                            GatewayError,
                        >,
                    > + Send,
            >,
        > {
            Box::pin(async {
                Err(GatewayError::Internal(
                    "TestProvider does not support streaming".to_string(),
                ))
            })
        }

        fn input_selectors(&self) -> &'static [&'static str] {
            &["textarea"]
        }

        fn submit_selectors(&self) -> &'static [&'static str] {
            &["button[type='submit']"]
        }

        fn response_selector(&self) -> &'static str {
            ".response"
        }

        fn thinking_selector(&self) -> Option<&'static str> {
            None
        }

        fn done_signal(&self) -> DoneSignal {
            DoneSignal::TextStable(Duration::from_millis(50))
        }
    }

    fn test_state() -> AppState {
        let mut registry = ProviderRegistry::new();
        registry
            .register(std::sync::Arc::new(TestProvider))
            .unwrap();
        AppState::new(Config::default(), registry, SessionManager::noop())
    }

    #[tokio::test]
    async fn health_check_is_unauthenticated() {
        let app = router(test_state());
        let response = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn models_requires_auth() {
        let app = router(test_state());
        let response = app
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn models_with_auth_succeeds() {
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::get("/v1/models")
                    .header("Authorization", "Bearer obscura-local")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_model_returns_bad_request() {
        let app = router(test_state());
        let body = Body::from(serde_json::json!({
            "model": "nonexistent",
            "messages": [{"role": "user", "content": "hi"}],
        }).to_string());
        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("Authorization", "Bearer obscura-local")
                    .header("Content-Type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
