//! SSE response construction for streaming endpoints.

use std::convert::Infallible;
use std::time::Duration;

use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use toolkit::api::canonical_prelude::CanonicalError;

use super::dto::render_event;
use crate::domain::error::DomainError;
use crate::domain::service::stream::TurnStream;

/// Turn a [`TurnStream`] into an SSE response (comment keep-alive every 30 s).
fn sse(ts: TurnStream) -> Response {
    let TurnStream { rx, guard } = ts;
    let stream = futures::stream::unfold((rx, guard), |(mut rx, guard)| async move {
        let ev = rx.recv().await?;
        let (name, data) = render_event(ev);
        let event = Event::default().event(name).data(data.to_string());
        Some((Ok::<Event, Infallible>(event), (rx, guard)))
    });
    sse_response(stream)
}

fn sse_response<S>(stream: S) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut resp = Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30))).into_response();
    resp.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-cache"));
    resp
}

/// Run the setup in a spawned task so a client disconnect cannot interrupt it.
///
/// # Errors
/// The canonical error of a rejected setup (no SSE stream is opened).
pub async fn detached<F>(fut: F) -> Result<Response, CanonicalError>
where
    F: std::future::Future<Output = Result<TurnStream, DomainError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(Ok(ts)) => Ok(sse(ts)),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(DomainError::internal(format!("stream setup task failed: {e}")).into()),
    }
}
