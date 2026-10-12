//! The SSE relay: turns the event channel of a turn into the `text/event-stream` response.
//!
//! `ping` is sent only between `stream_started` and the first `delta`/`tool` event, every
//! `sse_ping_interval_seconds` of idle time; the HTTP server adds an SSE comment keep-alive every
//! 30 s. The stream ends right after the terminal event; a channel that closes without one (the
//! provider task ended, for example because it lost the finalization CAS) ends with
//! `error{stream_interrupted}`. Dropping the response drops the channel, which cancels the turn.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::domain::quota::QuotaWarning;
use crate::domain::stream::{
    CitationItem, CitationSource, DeltaKind, DonePayload, EventStream, StreamEvent, ToolPhase,
};

/// Interval of the SSE comment keep-alive after content started.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Message of the synthesized `stream_interrupted` error.
const INTERRUPTED_MESSAGE: &str = "The stream ended before the turn reached a final state";

fn warning_json(warning: &QuotaWarning) -> Value {
    let mut value = json!({
        "tier": warning.tier,
        "period": warning.period,
        "remaining_percentage": warning.remaining_percentage,
        "warning": warning.warning,
        "exhausted": warning.exhausted,
    });
    if let Some(next_reset) = warning.next_reset
        && let Ok(text) = next_reset.format(&time::format_description::well_known::Rfc3339)
    {
        value["next_reset"] = Value::from(text);
    }
    value
}

fn done_json(done: &DonePayload) -> Value {
    let mut value = json!({
        "usage": { "input_tokens": done.input_tokens, "output_tokens": done.output_tokens },
        "effective_model": done.effective_model,
        "selected_model": done.selected_model,
        "quota_decision": done.quota_decision,
    });
    if let Some(from) = &done.downgrade_from {
        value["downgrade_from"] = Value::from(from.clone());
    }
    if let Some(reason) = done.downgrade_reason {
        value["downgrade_reason"] = Value::from(reason);
    }
    if let Some(warnings) = &done.quota_warnings {
        value["quota_warnings"] = Value::Array(warnings.iter().map(warning_json).collect());
    }
    value
}

fn citation_json(item: &CitationItem) -> Value {
    let mut value = json!({
        "source": match item.source {
            CitationSource::File => "file",
            CitationSource::Web => "web",
        },
        "title": item.title,
        "snippet": item.snippet,
    });
    if let Some(url) = &item.url {
        value["url"] = Value::from(url.clone());
    }
    if let Some(attachment_id) = item.attachment_id {
        value["attachment_id"] = Value::from(attachment_id.to_string());
    }
    if let Some((start, end)) = item.span {
        value["span"] = json!({ "start": start, "end": end });
    }
    value
}

/// The event name and JSON data of a stream event.
fn wire(event: &StreamEvent) -> (&'static str, Value) {
    match event {
        StreamEvent::Started {
            request_id,
            message_id,
            is_new_turn,
            thread_summary_tokens,
        } => {
            let mut value = json!({
                "request_id": request_id.to_string(),
                "message_id": message_id.to_string(),
                "is_new_turn": is_new_turn,
            });
            if let Some(tokens) = thread_summary_tokens {
                value["thread_summary_applied"] = json!({ "token_estimate": tokens });
            }
            ("stream_started", value)
        }
        StreamEvent::Delta { kind, content } => (
            "delta",
            json!({
                "type": match kind {
                    DeltaKind::Text => "text",
                    DeltaKind::Reasoning => "reasoning",
                },
                "content": content,
            }),
        ),
        StreamEvent::Tool {
            phase,
            name,
            details,
        } => (
            "tool",
            json!({
                "phase": match phase {
                    ToolPhase::Start => "start",
                    ToolPhase::Done => "done",
                },
                "name": name,
                "details": details,
            }),
        ),
        StreamEvent::Citations(items) => (
            "citations",
            json!({ "items": items.iter().map(citation_json).collect::<Vec<_>>() }),
        ),
        StreamEvent::Done(done) => ("done", done_json(done)),
        StreamEvent::Error { code, message } => {
            ("error", json!({ "code": code, "message": message }))
        }
    }
}

fn sse_event(name: &'static str, data: &Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

/// Where the relay is in the event order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing sent yet: no ping before `stream_started`.
    BeforeStart,
    /// `stream_started` sent, no `delta`/`tool` yet: idle time is filled with pings.
    Waiting,
    /// Content started: only the 30 s comment keep-alive of the HTTP server applies.
    Content,
    /// The terminal event was sent: the stream ends.
    Finished,
}

struct Relay {
    events: mpsc::Receiver<StreamEvent>,
    ping_interval: Duration,
    phase: Phase,
}

async fn next_event(mut relay: Relay) -> Option<(Result<Event, Infallible>, Relay)> {
    if relay.phase == Phase::Finished {
        return None;
    }
    let received = if relay.phase == Phase::Waiting {
        match timeout(relay.ping_interval, relay.events.recv()).await {
            Ok(received) => received,
            Err(_) => return Some((Ok(sse_event("ping", &json!({}))), relay)),
        }
    } else {
        relay.events.recv().await
    };
    let Some(event) = received else {
        relay.phase = Phase::Finished;
        let data = json!({ "code": "stream_interrupted", "message": INTERRUPTED_MESSAGE });
        return Some((Ok(sse_event("error", &data)), relay));
    };
    relay.phase = if event.is_terminal() {
        Phase::Finished
    } else if event.is_content() {
        Phase::Content
    } else {
        Phase::Waiting
    };
    let (name, data) = wire(&event);
    Some((Ok(sse_event(name, &data)), relay))
}

/// The relayed event stream of a turn.
pub fn relay(stream: EventStream) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    futures::stream::unfold(
        Relay {
            events: stream.events,
            ping_interval: stream.ping_interval,
            phase: Phase::BeforeStart,
        },
        next_event,
    )
}

/// The `text/event-stream` response of a turn: `Content-Type: text/event-stream`,
/// `Cache-Control: no-cache`, nothing buffered.
#[must_use]
pub fn sse_response(stream: EventStream) -> Response {
    Sse::new(relay(stream))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
        .into_response()
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
