//! Incremental server-sent events parsing and the provider event stream
//! driver shared by the adapters (ADR-0002 "HTTP with SSE passthrough",
//! DESIGN constraint "No Buffering").
//!
//! [`SseParser`] turns arbitrary byte chunks into [`SseEvent`]s as soon as an
//! event's terminating blank line arrives; lines and multi-byte characters may
//! be split anywhere between chunks, and `\n`, `\r\n` and `\r` line endings are
//! accepted. [`event_stream`] reads the OAGW response body chunk by chunk,
//! feeds the parser and hands every event to an adapter's
//! [`SseTranslator`]; the resulting [`LlmEventStream`] ends right after the
//! terminal event, when the body ends (with a synthesized failure when no
//! terminal event was seen), or when the cancellation token fires. The body is
//! dropped at that point, which aborts the provider request.

use std::collections::VecDeque;

use futures::StreamExt;
use oagw_sdk::body::BodyStream;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::domain::llm::{LlmEvent, LlmEventStream, LlmFailure};

/// Message of the failure synthesized when the body ends without a terminal
/// event.
pub const STREAM_ENDED_MESSAGE: &str = "provider stream ended unexpectedly";
/// Message of the failure synthesized when reading the body fails.
pub const STREAM_FAILED_MESSAGE: &str = "provider stream failed";

/// One parsed server-sent event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// Value of the `event:` field; `None` when the event had none.
    pub event: Option<String>,
    /// `data:` lines joined with `\n`.
    pub data: String,
}

/// Incremental SSE parser (W3C `EventSource` field rules; `id` and `retry` are
/// ignored).
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl SseParser {
    /// Feed a chunk and return the events it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        self.drain_lines(false)
    }

    /// Flush at end of body: a trailing event without its blank line is
    /// dispatched.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = self.drain_lines(true);
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.process_line(&line, &mut events);
        }
        self.dispatch(&mut events);
        events
    }

    fn drain_lines(&mut self, at_end: bool) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.buf[start..]
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
        {
            let end = start + offset;
            let consumed = match (self.buf[end], self.buf.get(end + 1)) {
                (b'\r', Some(b'\n')) => 2,
                // A trailing `\r` may be the first half of `\r\n`.
                (b'\r', None) if !at_end => break,
                _ => 1,
            };
            let line = self.buf[start..end].to_vec();
            self.process_line(&line, &mut events);
            start = end + consumed;
        }
        self.buf.drain(..start);
        events
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        // Line terminators are ASCII, so a complete line never splits a
        // multi-byte character.
        let line = String::from_utf8_lossy(line);
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        let event = self.event.take();
        let has_data = std::mem::take(&mut self.has_data);
        let data = std::mem::take(&mut self.data);
        if has_data || event.is_some() {
            events.push(SseEvent { event, data });
        }
    }
}

/// Adapter-specific translation of provider SSE events.
pub trait SseTranslator: Send + 'static {
    /// Translate one provider event; push zero or more internal events.
    fn translate(&mut self, event: SseEvent, out: &mut Vec<LlmEvent>);

    /// Terminal event when the body ended without one.
    fn stream_ended(&mut self) -> LlmEvent {
        LlmEvent::Failed(LlmFailure::provider_error(STREAM_ENDED_MESSAGE))
    }
}

/// Provider event stream over an OAGW response body, see the module
/// documentation.
pub fn event_stream<T: SseTranslator>(
    body: BodyStream,
    translator: T,
    cancel: CancellationToken,
) -> LlmEventStream {
    let driver = Driver {
        body: Some(body),
        parser: SseParser::default(),
        translator,
        pending: VecDeque::new(),
        cancel,
        finished: false,
    };
    Box::pin(futures::stream::unfold(driver, |mut driver| async move {
        driver.next_event().await.map(|event| (event, driver))
    }))
}

struct Driver<T> {
    body: Option<BodyStream>,
    parser: SseParser,
    translator: T,
    pending: VecDeque<LlmEvent>,
    cancel: CancellationToken,
    finished: bool,
}

/// Outcome of waiting for the next body chunk.
enum Read {
    Cancelled,
    Chunk(bytes::Bytes),
    Failed,
    Ended,
}

impl<T: SseTranslator> Driver<T> {
    async fn next_event(&mut self) -> Option<LlmEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            match self.read().await {
                Read::Cancelled => {
                    debug!(target: "mini_chat.llm", "provider stream cancelled");
                    self.close();
                    return None;
                }
                Read::Chunk(chunk) => {
                    let events = self.parser.push(&chunk);
                    self.translate_all(events);
                }
                Read::Failed => {
                    self.pending
                        .push_back(LlmEvent::Failed(LlmFailure::provider_error(
                            STREAM_FAILED_MESSAGE,
                        )));
                    self.close();
                }
                Read::Ended => {
                    let events = self.parser.finish();
                    self.translate_all(events);
                    if !self.finished {
                        let terminal = self.translator.stream_ended();
                        self.pending.push_back(terminal);
                        self.close();
                    }
                }
            }
        }
    }

    async fn read(&mut self) -> Read {
        let Some(body) = self.body.as_mut() else {
            return Read::Ended;
        };
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Read::Cancelled,
            chunk = body.next() => match chunk {
                Some(Ok(bytes)) => Read::Chunk(bytes),
                Some(Err(err)) => {
                    debug!(target: "mini_chat.llm", error = %err, "provider stream read failed");
                    Read::Failed
                }
                None => Read::Ended,
            },
        }
    }

    /// Translate parsed events, stopping at the first terminal event.
    fn translate_all(&mut self, events: Vec<SseEvent>) {
        let mut out = Vec::new();
        for event in events {
            self.translator.translate(event, &mut out);
            if let Some(position) = out.iter().position(LlmEvent::is_terminal) {
                out.truncate(position + 1);
                self.pending.extend(out);
                self.close();
                return;
            }
        }
        self.pending.extend(out);
    }

    /// Stop reading and drop the body (aborts the provider request).
    fn close(&mut self) {
        self.finished = true;
        self.body = None;
    }
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
