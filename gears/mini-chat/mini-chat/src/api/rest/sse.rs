//! SSE wire format (DESIGN §3.3 "SSE Event Definitions"): every event is
//! `event: <name>` + `data: <JSON payload>`.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt};
use serde::Serialize;
use tracing::error;

use super::dto::ErrorData;
pub use super::dto::MiniChatSseEvent as SseEvent;

/// Interval of the SSE comment keep-alive (`:` line) sent by axum.
pub const KEEP_ALIVE: Duration = Duration::from_secs(30);

impl SseEvent {
    /// SSE event name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::StreamStarted(_) => "stream_started",
            Self::Ping(_) => "ping",
            Self::Delta(_) => "delta",
            Self::Tool(_) => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error(_) => "error",
        }
    }

    /// `done` or `error`.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error(_))
    }

    /// `delta` or `tool` (content that ends the ping phase).
    #[must_use]
    pub const fn is_content(&self) -> bool {
        matches!(self, Self::Delta(_) | Self::Tool(_))
    }

    /// Terminal `error` event.
    #[must_use]
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error(ErrorData {
            code: code.to_owned(),
            message: message.into(),
        })
    }

    /// The axum SSE event (`event: <name>`, `data: <JSON>`).
    pub fn into_axum(self) -> Event {
        let name = self.name();
        let data = match &self {
            Self::StreamStarted(d) => to_json(d),
            Self::Ping(d) => to_json(d),
            Self::Delta(d) => to_json(d),
            Self::Tool(d) => to_json(d),
            Self::Citations(d) => to_json(d),
            Self::Done(d) => to_json(d),
            Self::Error(d) => to_json(d),
        };
        Event::default().event(name).data(data)
    }
}

fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|e| {
        error!(%e, "SSE payload serialization failed");
        "{}".to_owned()
    })
}

/// `text/event-stream` response over `events` with the 30 s comment keep-alive.
pub fn sse_response<S>(events: S) -> Response
where
    S: Stream<Item = SseEvent> + Send + 'static,
{
    Sse::new(events.map(|e| Ok::<_, Infallible>(e.into_axum())))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}
