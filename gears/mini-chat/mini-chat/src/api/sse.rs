//! SSE writer of the streaming responses (`messages:stream`, retry, edit).

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::{StreamExt as _, stream};
use http::HeaderValue;

use crate::domain::stream::{EventStream, StreamEvent};

/// Interval of the SSE comment keep-alive (a `:` line, no client-visible event).
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// `event: <name>` with the JSON `data` of `ev`.
pub fn into_sse_event(ev: &StreamEvent) -> Event {
    Event::default()
        .event(ev.event_name())
        .data(ev.data().to_string())
}

/// The `200 text/event-stream` response of `stream`: one SSE event per item, a comment
/// keep-alive every 30 s, `X-Accel-Buffering: no`. The body ends right after the first terminal
/// event (`done` or `error`).
#[must_use]
pub fn into_sse_response(stream: EventStream) -> Response {
    // After a terminal event the inner stream is not polled again: the body ends at once.
    let events = stream::unfold((stream, false), |(mut stream, ended)| async move {
        if ended {
            return None;
        }
        let ev = stream.next().await?;
        let ended = ev.is_terminal();
        Some((Ok::<_, Infallible>(into_sse_event(&ev)), (stream, ended)))
    })
    .boxed();
    let mut resp = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
        .into_response();
    resp.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn writes_named_events_and_stops_after_the_terminal_one() {
        let events: EventStream = Box::pin(stream::iter([
            StreamEvent::Ping,
            StreamEvent::Error {
                code: "provider_error".into(),
                message: "m".into(),
            },
            StreamEvent::Ping,
        ]));
        let resp = into_sse_response(events);
        assert_eq!(resp.headers()["x-accel-buffering"], "no");
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let body = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "event: ping\ndata: {}\n\nevent: error\ndata: {\"code\":\"provider_error\",\"message\":\"m\"}\n\n"
        );
    }

    /// After content starts there are no `ping` events; a `:` comment line every 30 s keeps the
    /// connection alive while the upstream is silent.
    #[tokio::test(start_paused = true)]
    async fn comment_keep_alive_every_30_seconds_while_the_upstream_is_silent() {
        let events: EventStream = Box::pin(
            stream::iter([StreamEvent::Delta {
                kind: crate::domain::stream::events::DeltaKind::Text,
                content: "a".into(),
            }])
            .chain(stream::pending()),
        );
        let mut body = into_sse_response(events).into_body().into_data_stream();
        let start = tokio::time::Instant::now();
        let mut next = async || {
            let chunk = tokio::time::timeout(Duration::from_secs(3600), body.next())
                .await
                .expect("a body chunk")
                .expect("the body stays open")
                .expect("chunk bytes");
            (start.elapsed(), String::from_utf8(chunk.to_vec()).unwrap())
        };

        let (at, delta) = next().await;
        assert_eq!(at, Duration::ZERO);
        assert_eq!(
            delta,
            "event: delta\ndata: {\"content\":\"a\",\"type\":\"text\"}\n\n"
        );
        for n in 1..=2 {
            let (at, keep_alive) = next().await;
            assert_eq!(at, KEEP_ALIVE_INTERVAL * n, "keep-alive {n}");
            assert!(keep_alive.starts_with(':'), "{keep_alive:?}");
            assert!(keep_alive.ends_with("\n\n"), "{keep_alive:?}");
            assert!(!keep_alive.contains("event:"), "{keep_alive:?}");
        }
    }

    #[tokio::test]
    async fn body_ends_after_the_terminal_event_while_the_upstream_stays_pending() {
        let events: EventStream = Box::pin(
            stream::iter([StreamEvent::Error {
                code: "provider_error".into(),
                message: "m".into(),
            }])
            .chain(stream::pending()),
        );
        let body = tokio::time::timeout(
            Duration::from_secs(5),
            to_bytes(into_sse_response(events).into_body(), 1 << 16),
        )
        .await
        .expect("the body ends right after the terminal event")
        .unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "event: error\ndata: {\"code\":\"provider_error\",\"message\":\"m\"}\n\n"
        );
    }
}
