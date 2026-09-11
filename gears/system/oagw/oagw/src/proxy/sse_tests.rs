//! Tests for server-sent-event relaying.

use bytes::Bytes;
use futures_util::StreamExt;

use crate::proxy::sse::{
    data_payloads, event_frame, is_event_stream, response_is_event_stream, SSE_CONTENT_TYPE,
};

#[test]
fn an_event_stream_is_recognised_by_its_media_type() {
    assert!(is_event_stream("text/event-stream"));
    assert!(is_event_stream("TEXT/EVENT-STREAM"));
    // Parameters do not change the media type.
    assert!(is_event_stream("text/event-stream; charset=utf-8"));
    assert!(!is_event_stream("application/json"));
    assert!(!is_event_stream("text/plain"));
    assert!(!is_event_stream(""));
}

#[test]
fn a_response_header_set_names_an_event_stream() {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    assert!(response_is_event_stream(&headers));

    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    assert!(!response_is_event_stream(&headers));

    assert!(!response_is_event_stream(&http::HeaderMap::new()));
}

#[test]
fn a_frame_carries_its_data_terminated_by_a_blank_line() {
    assert_eq!(event_frame(None, "hello"), "data: hello\n\n");
    assert_eq!(
        event_frame(Some("delta"), "hello"),
        "event: delta\ndata: hello\n\n"
    );
    // An empty event name is the unnamed event, not an empty field.
    assert_eq!(event_frame(Some(""), "hello"), "data: hello\n\n");
}

#[test]
fn a_multi_line_payload_becomes_several_data_fields() {
    assert_eq!(
        event_frame(None, "one\ntwo"),
        "data: one\ndata: two\n\n",
        "SSE folds a multi-line payload into repeated data fields"
    );
}

#[test]
fn the_payloads_of_a_buffered_chunk_are_recoverable() {
    let chunk = format!("{}{}", event_frame(Some("delta"), "one"), event_frame(None, "two"));
    assert_eq!(
        data_payloads(&chunk),
        vec!["one".to_owned(), "two".to_owned()]
    );
}

#[test]
fn a_partial_frame_at_the_end_of_a_chunk_is_still_reported() {
    // A stream hands the relay arbitrary byte boundaries, so a chunk can end mid-frame.
    let chunk = "data: one\n\ndata: two";
    assert_eq!(
        data_payloads(chunk),
        vec!["one".to_owned(), "two".to_owned()]
    );
}

#[test]
fn an_empty_stream_yields_no_payloads() {
    assert!(data_payloads("").is_empty());
    assert!(data_payloads("event: ping\n\n").is_empty());
}

/// The relay's core guarantee: each frame of an event stream reaches the caller as the
/// upstream produces it, without waiting for the stream to end.
#[tokio::test]
async fn a_stream_is_forwarded_incrementally() {
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::proxy::sse::StreamChunk>(4);
    let body = axum::body::Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(rx),
    );
    let mut stream = body.into_data_stream();

    tx.send(Ok(Bytes::from_static(b"data: one\n\n")))
        .await
        .expect("the channel accepts a frame");

    // The first frame is readable while the upstream is still running: the stream has
    // neither closed nor been buffered.
    let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("the first frame arrives without the stream closing")
        .expect("the chunk is readable");
    assert_eq!(
        first.expect("the chunk is readable"),
        Bytes::from_static(b"data: one\n\n")
    );

    // Nothing else was buffered ahead of it.
    drop(tx);
    let rest: Vec<_> = stream.collect().await;
    assert!(rest.is_empty(), "no further frames were buffered: {rest:?}");
}

/// The declared media type of the relayed response is preserved verbatim.
#[test]
fn the_content_type_is_preserved() {
    assert_eq!(SSE_CONTENT_TYPE, "text/event-stream");
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    assert_eq!(
        headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
}
