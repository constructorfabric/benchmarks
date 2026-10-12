//! Response wrappers of the harness.

use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::{HeaderMap, StatusCode};
use serde_json::Value;

/// A buffered HTTP response.
#[derive(Debug, Clone)]
pub struct TestResponse {
    /// Status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body.
    pub body: Bytes,
}

/// One parsed SSE frame.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    /// The `event:` name (`message` when absent).
    pub event: String,
    /// The `data:` payload, parsed as JSON (a JSON string when it is not valid JSON).
    pub data: Value,
}

impl TestResponse {
    /// The body parsed as JSON.
    ///
    /// # Panics
    /// Panics when the body is not JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!(
                "response body is not JSON ({err}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// The body parsed as SSE frames (`event:` / `data:` lines; `:` comment lines are ignored).
    pub fn sse_events(&self) -> Vec<SseEvent> {
        parse_sse(&String::from_utf8_lossy(&self.body))
    }
}

pub fn parse_sse(text: &str) -> Vec<SseEvent> {
    let normalized = text.replace("\r\n", "\n");
    normalized
        .split("\n\n")
        .filter_map(|frame| {
            let mut event = None;
            let mut data = Vec::new();
            for line in frame.lines() {
                if line.starts_with(':') {
                    continue;
                }
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
                }
            }
            if event.is_none() && data.is_empty() {
                return None;
            }
            let joined = data.join("\n");
            let data = serde_json::from_str(&joined).unwrap_or(Value::String(joined));
            Some(SseEvent {
                event: event.unwrap_or_else(|| "message".to_owned()),
                data,
            })
        })
        .collect()
}

/// The body of a response, as a stream of data chunks.
type BodyChunks = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>;

/// How long [`StreamingResponse::next_event`] waits for the next frame.
const NEXT_EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// A response whose body is still being produced.
pub struct StreamingResponse {
    /// Status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    body: BodyChunks,
    buffer: String,
    closed: bool,
}

impl StreamingResponse {
    pub(super) fn new(status: StatusCode, headers: HeaderMap, body: BodyChunks) -> Self {
        Self {
            status,
            headers,
            body,
            buffer: String::new(),
            closed: false,
        }
    }

    /// The next SSE event (`:` comment frames are skipped). `None` when the server closed the
    /// stream or when a bounded wait elapsed.
    pub async fn next_event(&mut self) -> Option<SseEvent> {
        loop {
            let normalized = self.buffer.replace("\r\n", "\n");
            if let Some(end) = normalized.find("\n\n") {
                let frame = normalized[..end].to_owned();
                normalized[end + 2..].clone_into(&mut self.buffer);
                if let Some(event) = parse_sse(&frame).into_iter().next() {
                    return Some(event);
                }
                continue;
            }
            if self.closed {
                return None;
            }
            match tokio::time::timeout(NEXT_EVENT_TIMEOUT, self.body.next()).await {
                Ok(Some(Ok(chunk))) => self.buffer.push_str(&String::from_utf8_lossy(&chunk)),
                Ok(Some(Err(_)) | None) => self.closed = true,
                Err(_) => return None,
            }
        }
    }

    /// Drops the response body: the client disconnect that cancels the turn.
    pub fn disconnect(self) {
        drop(self);
    }
}
