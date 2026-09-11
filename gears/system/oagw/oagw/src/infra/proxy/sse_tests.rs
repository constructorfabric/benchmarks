//! Unit tests for the SSE pass-through helpers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{
    EVENT_STREAM_MEDIA_TYPE, NO_BUFFERING_HEADER, SSE_DECORATION, count_events, decoration_for,
    idle_budget, is_event_stream, pass_through, split_events,
};
use std::time::Duration;

#[test]
fn a_text_event_stream_content_type_is_recognised() {
    assert!(is_event_stream("text/event-stream"));
    assert!(is_event_stream("text/event-stream; charset=utf-8"));
    assert!(is_event_stream("TEXT/EVENT-STREAM"));
    assert!(!is_event_stream("application/json"));
    assert!(!is_event_stream("text/event-streamx"));
    assert!(!is_event_stream(""));
}

#[test]
fn the_decoration_fills_only_the_missing_headers() {
    let upstream = vec![
        ("cache-control".to_owned(), "private".to_owned()),
        ("content-type".to_owned(), "text/event-stream".to_owned()),
    ];

    let added = decoration_for(&upstream);

    // The upstream's own cache-control stands; the other two are supplied.
    assert_eq!(
        added,
        vec![
            ("connection".to_owned(), "keep-alive".to_owned()),
            ("x-accel-buffering".to_owned(), "no".to_owned()),
        ]
    );
}

#[test]
fn an_undecorated_response_gets_every_header() {
    let added = decoration_for(&[]);

    assert_eq!(added.len(), SSE_DECORATION.len());
    assert!(added.contains(&("x-accel-buffering".to_owned(), "no".to_owned())));
    assert_eq!(EVENT_STREAM_MEDIA_TYPE, "text/event-stream");
    assert_eq!(NO_BUFFERING_HEADER, "x-accel-buffering");
}

#[test]
fn header_name_case_does_not_suppress_a_decoration() {
    let upstream = vec![("Cache-Control".to_owned(), "no-cache".to_owned())];

    assert_eq!(
        decoration_for(&upstream),
        vec![
            ("connection".to_owned(), "keep-alive".to_owned()),
            ("x-accel-buffering".to_owned(), "no".to_owned()),
        ]
    );
}

#[test]
fn a_partial_event_is_held_back() {
    let (events, rest) = split_events("data: hello\n\ndata: wor");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload, "data: hello\n\n");
    assert!(events[0].complete);
    assert_eq!(rest, "data: wor", "an unfinished event stays in the buffer");
}

#[test]
fn crlf_terminated_events_are_split_too() {
    let (events, _rest) = split_events("data: a\r\n\r\ndata: b\r\n\r\n");

    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.complete));
    assert_eq!(events[1].payload, "data: b\r\n\r\n");
}

#[test]
fn an_empty_buffer_yields_no_events() {
    assert_eq!(split_events(""), (Vec::new(), String::new()));
}

#[tokio::test]
async fn pass_through_surfaces_each_chunk_as_it_arrives() {
    use futures_util::StreamExt;

    let chunks = vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: one\n\n")),
        Ok(bytes::Bytes::from_static(b"data: two\n\n")),
    ];
    let body: crate::domain::services::proxy::BodyStream =
        Box::pin(futures_util::stream::iter(chunks));

    let mut streamed = pass_through(body);

    let first = streamed.next().await.unwrap().unwrap();
    assert_eq!(first, bytes::Bytes::from_static(b"data: one\n\n"));
    let second = streamed.next().await.unwrap().unwrap();
    assert_eq!(second, bytes::Bytes::from_static(b"data: two\n\n"));
    assert!(streamed.next().await.is_none());
}

#[test]
fn the_idle_budget_never_drops_below_a_second() {
    assert_eq!(
        idle_budget(Duration::from_millis(50)),
        Duration::from_secs(1)
    );
    assert_eq!(idle_budget(Duration::from_secs(7)), Duration::from_secs(7));
}

#[test]
fn event_counts_are_taken_from_the_terminators() {
    assert_eq!(count_events(b"data: a\n\ndata: b\n\n"), 2);
    assert_eq!(count_events(b"data: a\r\n\r\n"), 1);
    assert_eq!(count_events(b"data: a\n\n"), 1);
    assert_eq!(count_events(b""), 0);
}

/// The headers a proxied event stream carries back to the client, exactly as
/// the data plane assembles them.
fn streamed_response_headers(upstream: &[(String, String)]) -> Vec<(String, String)> {
    super::super::service::proxied_response_headers(
        upstream,
        &crate::domain::model::HeadersConfig::default(),
        true,
        None,
        "req-1",
    )
    .unwrap()
}

fn header_value<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[test]
fn a_stream_handed_through_carries_the_upstream_marker() {
    let pairs =
        streamed_response_headers(&[("content-type".to_owned(), "text/event-stream".to_owned())]);
    assert_eq!(
        header_value(&pairs, "x-oagw-error-source"),
        Some("upstream")
    );
    assert_eq!(
        header_value(&pairs, "content-type"),
        Some("text/event-stream")
    );
    assert_eq!(header_value(&pairs, "cache-control"), Some("no-cache"));
    assert_eq!(header_value(&pairs, "x-request-id"), Some("req-1"));
    assert_eq!(header_value(&pairs, "connection"), Some("keep-alive"));
    assert!(
        header_value(&pairs, "transfer-encoding").is_none(),
        "hop-by-hop headers do not survive"
    );
}

#[test]
fn a_stream_with_a_rate_limit_advertises_the_bucket() {
    let decision = crate::domain::ratelimit::RateDecision {
        allowed: true,
        limit: 2,
        remaining: 1,
        reset_seconds: 1,
    };
    let pairs = super::super::service::proxied_response_headers(
        &[("content-type".to_owned(), "text/event-stream".to_owned())],
        &crate::domain::model::HeadersConfig::default(),
        true,
        Some(&decision),
        "req-1",
    )
    .unwrap();
    assert_eq!(header_value(&pairs, "x-ratelimit-limit"), Some("2"));
    assert_eq!(
        header_value(&pairs, "x-oagw-error-source"),
        Some("upstream")
    );
}

#[test]
fn a_gateway_failure_during_a_stream_is_marked_gateway() {
    let error = crate::domain::error::OagwError::StreamAborted(
        "the upstream connection closed before the stream finished".to_owned(),
    );
    assert_eq!(error.status(), 502);
    assert_eq!(
        error.type_id(),
        crate::gts_helpers::error_type_id("stream.aborted")
    );
    let response = crate::api::rest::error::gateway_error_response(&error, None);
    assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
}

#[test]
fn a_transport_failure_while_streaming_is_retriable() {
    let timeout = crate::domain::error::OagwError::ConnectionTimeout("slow".to_owned());
    assert_eq!(timeout.status(), 504);
    assert_eq!(
        timeout.type_id(),
        crate::gts_helpers::error_type_id("timeout.connection")
    );
    assert!(
        timeout.retry_after().is_some(),
        "the client is told to come back"
    );
}
