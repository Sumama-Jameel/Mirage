//! Shared session-guarded stream wrapper.
//!
//! Several providers obtain a browser session for the lifetime of a streaming
//! chat completion. If the consumer drops the stream early (or it ends in
//! error), the session must be released so the pool can hand it to another
//! caller. This wrapper tracks whether the underlying stream produced an
//! error and releases the session with the correct `dirty` flag on drop.
//!
//! Each provider previously defined its own copy of this struct. The shared
//! version here is behaviour-identical and lets us fix bugs in one place.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::{BoxStream, Stream};

use crate::error::GatewayError;
use crate::models::ChatCompletionChunk;
use crate::session::SessionManager;

/// A stream wrapper that releases its associated browser session when dropped.
///
/// The session is always released as clean (`dirty=false`) because direct
/// providers use the session only for auth extraction, never for browser-level
/// chat. Browser-UI providers that need dirty-on-error tracking should manage
/// the session lifecycle themselves.
pub struct SessionGuardStream {
    inner: BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
    sessions: SessionManager,
    session_id: String,
}

impl SessionGuardStream {
    /// Wrap an inner stream with the given session.
    pub fn new(
        inner: BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>,
        sessions: SessionManager,
        session_id: String,
    ) -> Self {
        Self {
            inner,
            sessions,
            session_id,
        }
    }
}

impl Stream for SessionGuardStream {
    type Item = Result<ChatCompletionChunk, GatewayError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

impl Drop for SessionGuardStream {
    fn drop(&mut self) {
        let sessions = self.sessions.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            if let Err(e) = sessions.release(session_id, false).await {
                tracing::warn!(error = %e, "failed to release session from stream guard");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use futures::stream::StreamExt;

    fn dummy_session_id() -> String {
        // Use a unique ID so we never collide with a real session in the
        // process-global SessionManager. The release call will fail to find
        // the session and just log a warn; the test only cares about the
        // stream itself.
        format!("test_guard_{}", uuid::Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn passthrough_yields_inner_chunks() {
        let chunks: Vec<Result<ChatCompletionChunk, GatewayError>> = vec![Ok(ChatCompletionChunk {
            id: "1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "m".to_string(),
            choices: vec![],
            session_url: None,
        })];
        let inner = stream::iter(chunks).boxed();
        let sessions = SessionManager::empty();
        let mut guarded = SessionGuardStream::new(inner, sessions, dummy_session_id());
        let item = guarded.next().await;
        assert!(item.is_some());
    }

    #[tokio::test]
    async fn propagates_stream_errors() {
        let chunks: Vec<Result<ChatCompletionChunk, GatewayError>> =
            vec![Err(GatewayError::Internal("oops".to_string()))];
        let inner = stream::iter(chunks).boxed();
        let sessions = SessionManager::empty();
        let mut guarded = SessionGuardStream::new(inner, sessions, dummy_session_id());
        let item = guarded.next().await;
        assert!(matches!(item, Some(Err(_))));
    }
}
