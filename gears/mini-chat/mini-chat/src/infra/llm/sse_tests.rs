use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use oagw_sdk::body::{BodyStream, BoxError};
use tokio_util::sync::CancellationToken;

use super::{
    STREAM_ENDED_MESSAGE, STREAM_FAILED_MESSAGE, SseEvent, SseParser, SseTranslator, event_stream,
};
use crate::domain::llm::{LlmEvent, LlmFailure};

fn ev(event: Option<&str>, data: &str) -> SseEvent {
    SseEvent {
        event: event.map(str::to_owned),
        data: data.to_owned(),
    }
}

fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.push(chunk));
    }
    events.extend(parser.finish());
    events
}

const WIRE: &str = "event: response.created\ndata: {\"a\":1}\n\n\
                    : comment line\n\
                    event: response.output_text.delta\r\ndata: {\"delta\":\"h\u{e9}llo\"}\r\n\r\n\
                    data: line1\ndata: line2\n\n\
                    event: done\n\n";

fn expected_wire_events() -> Vec<SseEvent> {
    vec![
        ev(Some("response.created"), "{\"a\":1}"),
        ev(
            Some("response.output_text.delta"),
            "{\"delta\":\"h\u{e9}llo\"}",
        ),
        ev(None, "line1\nline2"),
        ev(Some("done"), ""),
    ]
}

#[test]
fn parser_handles_every_split_point() {
    let bytes = WIRE.as_bytes();
    for split in 0..=bytes.len() {
        let (head, tail) = bytes.split_at(split);
        assert_eq!(
            parse_all(&[head, tail]),
            expected_wire_events(),
            "split at {split}"
        );
    }
}

#[test]
fn parser_handles_byte_by_byte_chunks() {
    let chunks: Vec<&[u8]> = WIRE.as_bytes().chunks(1).collect();
    assert_eq!(parse_all(&chunks), expected_wire_events());
}

#[test]
fn parser_accepts_bare_cr_and_field_without_space() {
    let events = parse_all(&[b"event:x\rdata:1\r\r", b"data: 2\r"]);
    assert_eq!(events, vec![ev(Some("x"), "1"), ev(None, "2")]);
}

#[test]
fn parser_flushes_trailing_event_without_blank_line() {
    assert_eq!(parse_all(&[b"data: tail"]), vec![ev(None, "tail")]);
    assert!(parse_all(&[b": only a comment\n\n"]).is_empty());
}

#[test]
fn parser_emits_events_before_the_body_ends() {
    let mut parser = SseParser::default();
    assert!(parser.push(b"data: first").is_empty());
    assert_eq!(parser.push(b"\n\ndata: sec"), vec![ev(None, "first")]);
}

/// Test translator: `data` becomes a delta, `end` completes, `fail` fails.
struct EchoTranslator;

impl SseTranslator for EchoTranslator {
    fn translate(&mut self, event: SseEvent, out: &mut Vec<LlmEvent>) {
        match event.data.as_str() {
            "end" => out.push(LlmEvent::Completed {
                response_id: None,
                usage: None,
                incomplete_reason: None,
                output_text: String::new(),
            }),
            "fail" => out.push(LlmEvent::Failed(LlmFailure::provider_error("failed"))),
            data => out.push(LlmEvent::TextDelta(data.to_owned())),
        }
    }
}

fn body(chunks: Vec<Result<&'static str, &'static str>>) -> BodyStream {
    let items: Vec<Result<Bytes, BoxError>> = chunks
        .into_iter()
        .map(|chunk| match chunk {
            Ok(text) => Ok(Bytes::from_static(text.as_bytes())),
            Err(err) => Err(BoxError::from(err)),
        })
        .collect();
    Box::pin(futures::stream::iter(items))
}

async fn collect(body: BodyStream) -> Vec<LlmEvent> {
    event_stream(body, EchoTranslator, CancellationToken::new())
        .collect()
        .await
}

fn delta(text: &str) -> LlmEvent {
    LlmEvent::TextDelta(text.to_owned())
}

fn failed(message: &str) -> LlmEvent {
    LlmEvent::Failed(LlmFailure::provider_error(message))
}

#[tokio::test]
async fn stream_stops_after_the_terminal_event() {
    let events = collect(body(vec![Ok("data: a\n\ndata: end\n\ndata: ignored\n\n")])).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], delta("a"));
    assert!(events[1].is_terminal());
}

#[tokio::test]
async fn stream_without_terminal_event_fails() {
    let events = collect(body(vec![Ok("data: a\n"), Ok("\n")])).await;
    assert_eq!(events, vec![delta("a"), failed(STREAM_ENDED_MESSAGE)]);
}

#[tokio::test]
async fn trailing_terminal_event_without_blank_line_is_used() {
    let events = collect(body(vec![Ok("data: a\n\ndata: fail")])).await;
    assert_eq!(events, vec![delta("a"), failed("failed")]);
}

#[tokio::test]
async fn body_error_fails_the_stream() {
    let events = collect(body(vec![Ok("data: a\n\n"), Err("reset")])).await;
    assert_eq!(events, vec![delta("a"), failed(STREAM_FAILED_MESSAGE)]);
}

/// Body that never ends and records when it is dropped.
struct PendingBody {
    first: Option<Bytes>,
    dropped: Arc<AtomicBool>,
}

impl Stream for PendingBody {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.first.take() {
            Some(bytes) => Poll::Ready(Some(Ok(bytes))),
            None => Poll::Pending,
        }
    }
}

impl Drop for PendingBody {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

fn pending_body(dropped: &Arc<AtomicBool>) -> BodyStream {
    Box::pin(PendingBody {
        first: Some(Bytes::from_static(b"data: a\n\n")),
        dropped: Arc::clone(dropped),
    })
}

#[tokio::test]
async fn cancellation_ends_the_stream_and_drops_the_body() {
    let dropped = Arc::new(AtomicBool::new(false));
    let cancel = CancellationToken::new();
    let mut stream = event_stream(pending_body(&dropped), EchoTranslator, cancel.clone());

    assert_eq!(stream.next().await, Some(delta("a")));
    cancel.cancel();
    assert_eq!(stream.next().await, None);
    assert!(
        dropped.load(Ordering::SeqCst),
        "body must be dropped on cancel"
    );
}

#[tokio::test]
async fn dropping_the_stream_drops_the_body() {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut stream = event_stream(
        pending_body(&dropped),
        EchoTranslator,
        CancellationToken::new(),
    );
    assert_eq!(stream.next().await, Some(delta("a")));
    assert!(!dropped.load(Ordering::SeqCst));
    drop(stream);
    assert!(
        dropped.load(Ordering::SeqCst),
        "body must be dropped with the stream"
    );
}
