//! SSE handlers: `messages:stream`, turn retry and turn edit.
//!
//! The setup runs in a spawned task so a client disconnect before the stream
//! opens does not interrupt it; dropping the SSE body cancels the turn.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::http::{HeaderValue, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use futures::stream;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{EditTurnRequest, StreamMessageRequest};
use super::handlers::Svc;
use crate::domain::error::DomainError;
use crate::domain::stream::StreamStart;
use crate::domain::stream::events::{StreamEvent, StreamStarted};
use crate::domain::stream::setup::SendRequest;

fn to_event(ev: &StreamEvent) -> Event {
    Event::default().event(ev.name()).data(ev.data())
}

struct Relay {
    started: Option<StreamStarted>,
    rx: mpsc::Receiver<StreamEvent>,
    _guard: DropGuard,
    ping: Duration,
    content_started: bool,
    finished: bool,
}

fn live_stream(
    started: StreamStarted,
    rx: mpsc::Receiver<StreamEvent>,
    cancel: CancellationToken,
    ping: Duration,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let state = Relay {
        started: Some(started),
        rx,
        _guard: cancel.drop_guard(),
        ping,
        content_started: false,
        finished: false,
    };
    stream::unfold(state, |mut s| async move {
        if s.finished {
            return None;
        }
        if let Some(st) = s.started.take() {
            return Some((Ok(to_event(&StreamEvent::Started(st))), s));
        }
        let next = if s.content_started {
            s.rx.recv().await
        } else {
            match tokio::time::timeout(s.ping, s.rx.recv()).await {
                Err(_) => return Some((Ok(to_event(&StreamEvent::Ping)), s)),
                Ok(n) => n,
            }
        };
        if let Some(ev) = next {
            if ev.is_content() {
                s.content_started = true;
            }
            if ev.is_terminal() {
                s.finished = true;
            }
            Some((Ok(to_event(&ev)), s))
        } else {
            s.finished = true;
            let ev =
                StreamEvent::error("stream_interrupted", "The response stream was interrupted");
            Some((Ok(to_event(&ev)), s))
        }
    })
}

fn sse_response<S>(s: S) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut resp = Sse::new(s)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

fn respond(svc: &Svc, start: StreamStart) -> Response {
    match start {
        StreamStart::Replay(events) => {
            let items: Vec<Result<Event, Infallible>> =
                events.iter().map(|e| Ok(to_event(e))).collect();
            sse_response(stream::iter(items))
        }
        StreamStart::Live {
            started,
            rx,
            cancel,
        } => {
            let ping = Duration::from_secs(u64::from(svc.cfg.streaming.sse_ping_interval_seconds));
            sse_response(live_stream(started, rx, cancel, ping))
        }
    }
}

async fn run_setup<F>(fut: F) -> Result<StreamStart, CanonicalError>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(CanonicalError::internal(format!("stream setup task failed: {e}")).create()),
    }
}

pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let send = SendRequest {
        content: req.content,
        request_id: req.request_id,
        attachment_ids: req.attachment_ids.unwrap_or_default(),
        web_search: req.web_search.is_some_and(|w| w.enabled),
    };
    let s = Arc::clone(&svc);
    let start = run_setup(async move { s.start_send(ctx, id, send).await }).await?;
    Ok(respond(&svc, start))
}

pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let s = Arc::clone(&svc);
    let start = run_setup(async move { s.start_mutation(ctx, id, request_id, None).await }).await?;
    Ok(respond(&svc, start))
}

pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let s = Arc::clone(&svc);
    let start = run_setup(async move {
        s.start_mutation(ctx, id, request_id, Some(req.content))
            .await
    })
    .await?;
    Ok(respond(&svc, start))
}
