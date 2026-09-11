//! Server-Sent Events pass-through.
//!
//! An SSE response is never buffered and never re-framed: the upstream's bytes
//! are handed to the client as they arrive, with the headers a browser expects
//! from a proxied event stream. The only inspection here decides *whether* a
//! response is an event stream, so the service can decorate it and account for
//! it as a stream.

use crate::domain::services::proxy::BodyStream;
use std::time::Duration;

/// The content type that marks a response as an event stream.
pub const EVENT_STREAM_MEDIA_TYPE: &str = "text/event-stream";

/// Header set on proxied streams so intermediaries do not buffer them.
pub const NO_BUFFERING_HEADER: &str = "x-accel-buffering";

/// Headers an SSE response must carry after the gateway has seen it.
pub const SSE_DECORATION: [(&str, &str); 3] = [
    ("cache-control", "no-cache"),
    ("connection", "keep-alive"),
    ("x-accel-buffering", "no"),
];

/// Whether a response content type is an event stream.
#[must_use]
pub fn is_event_stream(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .map(str::trim)
        .is_some_and(|media| media.eq_ignore_ascii_case(EVENT_STREAM_MEDIA_TYPE))
}

/// The headers an SSE response must carry, without clobbering upstream values.
///
/// Returns only the pairs the upstream did not already set.
#[must_use]
pub fn decoration_for(headers: &[(String, String)]) -> Vec<(String, String)> {
    SSE_DECORATION
        .iter()
        .filter(|(name, _)| {
            !headers
                .iter()
                .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
        })
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// Wraps a body so each upstream chunk is surfaced as its own frame.
///
/// The mapping is deliberately one-to-one: no chunk is held back waiting for a
/// complete event, which is what "no buffering of the event stream" means.
#[must_use]
pub fn pass_through(body: BodyStream) -> BodyStream {
    Box::pin(body)
}

/// An event observed on a proxied stream, for the metrics and for the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    /// Bytes handed downstream.
    pub payload: String,
    /// Whether the payload carried a complete event (a blank-line terminator).
    pub complete: bool,
}

/// Splits a buffer into the events it already terminates.
///
/// A complete event ends with a blank line (`\n\n`, `\r\n\r\n` or a lone `\r`
/// at the very end). Anything after the last terminator is left in the buffer,
/// because it belongs to an event the upstream has not finished writing.
#[must_use]
pub fn split_events(buffer: &str) -> (Vec<StreamEvent>, String) {
    let mut events = Vec::new();
    let mut rest = buffer;
    while let Some(index) = find_terminator(rest) {
        let (chunk, tail) = rest.split_at(index + terminator_len(rest, index));
        events.push(StreamEvent {
            payload: chunk.to_owned(),
            complete: true,
        });
        rest = tail;
    }
    (events, rest.to_owned())
}

fn find_terminator(buffer: &str) -> Option<usize> {
    let crlf = buffer.find("\r\n\r\n");
    let lf = buffer.find("\n\n");
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn terminator_len(buffer: &str, index: usize) -> usize {
    if buffer[index..].starts_with("\r\n\r\n") {
        4
    } else {
        2
    }
}

/// The per-chunk read budget for a stream that has gone quiet.
#[must_use]
pub fn idle_budget(configured: Duration) -> Duration {
    configured.max(Duration::from_secs(1))
}

/// Counts the events in a byte slice, for the stream metrics.
#[must_use]
pub fn count_events(payload: &[u8]) -> usize {
    let text = String::from_utf8_lossy(payload);
    text.match_indices("\n\n")
        .count()
        .saturating_add(text.match_indices("\r\n\r\n").count())
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
