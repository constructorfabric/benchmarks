//! SSE client helpers for integration tests: send a streaming request through the
//! in-process router and read the `event:` / `data:` frames of the response.

use std::collections::VecDeque;
use std::time::Duration;

use axum::body::{Body, BodyDataStream, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use futures::StreamExt;
use serde_json::Value;

use super::harness::TestApp;
use super::users::TestUser;

/// Longest wait for one SSE event (or the end of the stream).
const EVENT_WAIT: Duration = Duration::from_secs(30);

/// Parse complete SSE frames: `(event name, data)`; `data` is JSON when it
/// parses, else a string. Comment-only frames (keep-alives) are skipped.
#[must_use]
pub fn parse_sse(body: &str) -> Vec<(String, Value)> {
    let normalized = body.replace("\r\n", "\n");
    normalized.split("\n\n").filter_map(parse_frame).collect()
}

fn parse_frame(frame: &str) -> Option<(String, Value)> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim_start().to_owned());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    if event.is_none() && data.is_empty() {
        return None;
    }
    let data = data.join("\n");
    let value = serde_json::from_str(&data).unwrap_or(Value::String(data));
    Some((event.unwrap_or_else(|| "message".to_owned()), value))
}

/// A whole streaming response.
#[derive(Debug)]
pub struct SseCapture {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// Events of a `text/event-stream` response (empty otherwise).
    pub events: Vec<(String, Value)>,
    /// JSON body of a non-SSE (problem) response.
    pub problem: Option<Value>,
}

impl SseCapture {
    /// Names of the events, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// Data of the first event named `name`.
    #[must_use]
    pub fn first(&self, name: &str) -> Option<&Value> {
        self.events.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Data of the last event (the terminal one on a well-formed stream).
    #[must_use]
    pub fn last(&self) -> Option<&(String, Value)> {
        self.events.last()
    }
}

/// An open SSE response; dropping it disconnects the client.
pub struct DropHandle {
    pub status: StatusCode,
    pub headers: HeaderMap,
    body: BodyDataStream,
    buf: String,
    pending: VecDeque<(String, Value)>,
    ended: bool,
}

impl DropHandle {
    fn new(status: StatusCode, headers: HeaderMap, body: Body) -> Self {
        Self {
            status,
            headers,
            body: body.into_data_stream(),
            buf: String::new(),
            pending: VecDeque::new(),
            ended: false,
        }
    }

    /// Next event; `None` when the server closed the stream.
    ///
    /// # Panics
    /// When no event arrives within 30 s or the body fails.
    #[allow(clippy::expect_used)]
    pub async fn next_event(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(ev);
            }
            if self.ended {
                return None;
            }
            let chunk = tokio::time::timeout(EVENT_WAIT, self.body.next())
                .await
                .expect("timed out waiting for an SSE event");
            let Some(bytes) = chunk else {
                let rest = std::mem::take(&mut self.buf);
                self.pending.extend(parse_frame(&rest));
                self.ended = true;
                continue;
            };
            let bytes = bytes.expect("read SSE body");
            self.buf
                .push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
            while let Some(pos) = self.buf.find("\n\n") {
                let frame: String = self.buf.drain(..pos + 2).collect();
                self.pending.extend(parse_frame(&frame));
            }
        }
    }

    /// Every remaining event until the server closes the stream.
    pub async fn rest(mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Some(ev) = self.next_event().await {
            out.push(ev);
        }
        out
    }
}

/// `POST path` with a JSON body as `user`.
///
/// # Panics
/// When the request cannot be built.
#[allow(clippy::expect_used)]
#[must_use]
pub fn json_request(user: TestUser, method: Method, path: &str, body: &Value) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(body).expect("serialize body"),
        ))
        .expect("build request");
    req.extensions_mut().insert(user);
    req
}

fn is_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
}

impl TestApp {
    /// `POST path` (a streaming endpoint) and read the whole response.
    ///
    /// # Panics
    /// When the response does not finish within 30 s per event.
    #[allow(clippy::expect_used)]
    pub async fn stream(&self, user: TestUser, path: &str, body: Value) -> SseCapture {
        self.stream_with(user, Method::POST, path, body).await
    }

    /// Like [`Self::stream`] with an explicit method (turn edit is `PATCH`).
    ///
    /// # Panics
    /// When the response does not finish within 30 s per event.
    #[allow(clippy::expect_used)]
    pub async fn stream_with(
        &self,
        user: TestUser,
        method: Method,
        path: &str,
        body: Value,
    ) -> SseCapture {
        let resp = tokio::time::timeout(
            EVENT_WAIT,
            self.raw(json_request(user, method, path, &body)),
        )
        .await
        .expect("timed out waiting for the response head");
        let (parts, body) = resp.into_parts();
        if is_sse(&parts.headers) {
            let conn = DropHandle::new(parts.status, parts.headers.clone(), body);
            let events = conn.rest().await;
            SseCapture {
                status: parts.status,
                headers: parts.headers,
                events,
                problem: None,
            }
        } else {
            let bytes = to_bytes(body, usize::MAX).await.expect("read body");
            SseCapture {
                status: parts.status,
                headers: parts.headers,
                events: Vec::new(),
                problem: Some(serde_json::from_slice(&bytes).unwrap_or(Value::Null)),
            }
        }
    }

    /// `POST path`, read the first `n` events and keep the connection open: the
    /// returned [`DropHandle`] reads further events, dropping it disconnects.
    ///
    /// # Panics
    /// When the response is not an SSE stream or the events do not arrive.
    #[allow(clippy::expect_used)]
    pub async fn stream_until(
        &self,
        user: TestUser,
        path: &str,
        body: Value,
        n: usize,
    ) -> (Vec<(String, Value)>, DropHandle) {
        let resp = tokio::time::timeout(
            EVENT_WAIT,
            self.raw(json_request(user, Method::POST, path, &body)),
        )
        .await
        .expect("timed out waiting for the response head");
        let (parts, body) = resp.into_parts();
        if !is_sse(&parts.headers) {
            let bytes = to_bytes(body, usize::MAX).await.unwrap_or_default();
            panic!(
                "expected an SSE response, got {}: {}",
                parts.status,
                String::from_utf8_lossy(&bytes)
            );
        }
        let mut conn = DropHandle::new(parts.status, parts.headers, body);
        let mut events = Vec::with_capacity(n);
        while events.len() < n {
            let ev = conn
                .next_event()
                .await
                .expect("stream ended before the expected events");
            events.push(ev);
        }
        (events, conn)
    }
}
