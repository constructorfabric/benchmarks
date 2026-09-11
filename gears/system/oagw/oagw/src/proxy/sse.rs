//! Server-sent-event relaying.
//!
//! An SSE response is relayed as a byte stream: the gateway never buffers the event
//! sequence in order to re-frame it, because a stream that outlives the caller's patience
//! would then never deliver anything. This module carries the two decisions the relay has
//! to make — whether a response is an event stream at all, and how a frame is rendered —
//! so the streaming behaviour itself stays in [`crate::proxy::relay_response`], which
//! hands the upstream body straight to `Body::from_stream`.

use bytes::Bytes;
use http::HeaderMap;

/// The content type an SSE response carries.
pub const SSE_CONTENT_TYPE: &str = "text/event-stream";

/// Whether a response is a server-sent-event stream.
#[must_use]
pub fn is_event_stream(content_type: &str) -> bool {
    let lowered = content_type.to_ascii_lowercase();
    let media = lowered.split(';').next().unwrap_or_default().trim();
    media == SSE_CONTENT_TYPE
}

/// Whether a response header set names an event stream.
#[must_use]
pub fn response_is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_event_stream)
}

/// Renders one server-sent-event frame.
///
/// An empty `event` omits the field entirely, which is what a client expects from an
/// unnamed event.
#[must_use]
pub fn event_frame(event: Option<&str>, data: &str) -> String {
    let mut frame = String::new();
    if let Some(event) = event.filter(|name| !name.is_empty()) {
        frame.push_str("event: ");
        frame.push_str(event);
        frame.push('\n');
    }
    for line in data.split('\n') {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    frame
}

/// Extracts the `data:` payloads of the frames buffered in `chunk`.
///
/// The relay does not parse the stream — it forwards bytes — but the gateway uses this to
/// assert that a chunk it handed on was complete, which is what the smoke test asserts.
#[must_use]
pub fn data_payloads(chunk: &str) -> Vec<String> {
    let mut payloads = Vec::new();
    let mut current = String::new();
    let mut started = false;
    for line in chunk.split('\n') {
        if let Some(data) = line.strip_prefix("data:") {
            started = true;
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(data.strip_prefix(' ').unwrap_or(data));
        } else if line.is_empty() && started {
            payloads.push(std::mem::take(&mut current));
            started = false;
        }
    }
    if started && !current.is_empty() {
        payloads.push(current);
    }
    payloads
}

/// A byte chunk of an event stream.
pub type StreamChunk = Result<Bytes, std::io::Error>;

