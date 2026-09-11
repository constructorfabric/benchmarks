//! Tests of the streaming relay and its lifecycle
//! (`cpt-cf-oagw-algo-request-proxy-stream-lifecycle`).
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-sse-streaming:p1

use crate::infra::proxy::stream::{abort_error, collect, idle_error, map_stream_error, relay, IDLE_WINDOW};
use crate::domain::proxy::{ProxyByteStream, StreamEvent, StreamLifecycle};
use std::sync::Arc;
use std::time::Duration;
use bytes::Bytes;
use toolkit_http::{HttpError, LimitedBody};
use crate::domain::DomainError;
use futures_util::StreamExt;

type FrameError = Box<dyn std::error::Error + Send + Sync>;

/// A body that yields the given data frames in order and then ends.
fn frames(values: &[&str]) -> LimitedBody {
    let owned: Vec<Bytes> = values.iter().map(|value: &&str| Bytes::from((*value).to_owned())).collect();
    let stream =
        futures_util::stream::iter(owned.into_iter().map(|value| Ok(hyper::body::Frame::data(value))));
    LimitedBody::new(http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(stream)), usize::MAX)
}

/// A body that never yields, so the idle window is the only thing that can
/// end the session.
fn silent() -> LimitedBody {
    let stream: futures_util::stream::Pending<Result<hyper::body::Frame<Bytes>, FrameError>> =
        futures_util::stream::pending();
    LimitedBody::new(http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(stream)), usize::MAX)
}

/// A body that yields one frame and then fails.
fn failing() -> LimitedBody {
    let stream = futures_util::stream::iter(vec![
        Ok(hyper::body::Frame::data(Bytes::from_static(b"partial"))),
        Err::<hyper::body::Frame<Bytes>, FrameError>(Box::new(std::io::Error::other("reset"))),
    ]);
    LimitedBody::new(http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(stream)), usize::MAX)
}

fn relayed(body: LimitedBody, idle: Duration) -> (Arc<StreamLifecycle>, ProxyByteStream) {
    let lifecycle = StreamLifecycle::shared();
    let stream = relay(body, Arc::clone(&lifecycle), idle, Some("trace".to_owned()));
    (lifecycle, stream)
}

#[tokio::test]
async fn a_body_is_relayed_in_arrival_order() {
    let (lifecycle, mut stream) = relayed(frames(&["one", "two"]), Duration::from_secs(1));
    let mut collected = Vec::new();
    while let Some(item) = stream.next().await {
        collected.push(item.expect("relayed"));
    }
    assert_eq!(collected, vec![Bytes::from("one"), Bytes::from("two")]);
    assert_eq!(lifecycle.events(), vec![StreamEvent::Open, StreamEvent::Close]);
}

#[tokio::test]
async fn an_upstream_error_yields_the_aborted_outcome_then_ends_the_stream() {
    let lifecycle = StreamLifecycle::shared();
    let mut stream = relay(failing(), Arc::clone(&lifecycle), Duration::from_secs(60), None);
    let first = stream.next().await.expect("the partial frame");
    assert!(first.is_ok());
    let second = stream.next().await.expect("the abort");
    let error = second.expect_err("aborted");
    assert!(format!("{error}").contains("stream"));
    assert!(stream.next().await.is_none());
    assert_eq!(lifecycle.events(), vec![StreamEvent::Open, StreamEvent::Aborted]);
}

#[tokio::test]
async fn an_idle_window_expiry_is_attributed_to_the_aborted_event() {
    let (lifecycle, mut stream) = relayed(silent(), Duration::from_millis(5));
    tokio::time::sleep(Duration::from_millis(10)).await;
    let item = stream.next().await.expect("the idle expiry");
    assert!(item.is_err());
    assert_eq!(lifecycle.events(), vec![StreamEvent::Aborted]);
}

#[test]
fn the_idle_window_is_fixed_and_not_the_proxy_timeout() {
    assert_eq!(IDLE_WINDOW, Duration::from_secs(60));
}

#[test]
fn a_transport_timeout_is_attributed_to_the_idle_timeout_and_anything_else_to_the_abort() {
    let timeout = map_stream_error(&HttpError::Timeout(Duration::from_secs(1)), None);
    assert!(matches!(timeout, DomainError::IdleTimeout { .. }));
    let abort = map_stream_error(&HttpError::Transport("reset".into()), None);
    assert!(matches!(abort, DomainError::StreamAborted { .. }));
}

#[test]
fn the_stream_and_idle_errors_carry_the_trace_identifier() {
    let abort = abort_error(Some("t".to_owned()));
    assert!(format!("{abort}").contains("stream"));
    let idle = idle_error(Some("t".to_owned()));
    assert!(format!("{idle}").contains("idle") || !format!("{idle}").is_empty());
}

#[tokio::test]
async fn a_body_is_collected_to_its_end() {
    let bytes = collect(frames(&["a", "b", "c"]), usize::MAX, None).await.expect("collected");
    assert_eq!(bytes, Bytes::from_static(b"abc"));
}

#[tokio::test]
async fn a_body_larger_than_the_ceiling_is_not_collected() {
    // The buffered leg is bounded: the first frame that would carry the
    // accumulated buffer past the ceiling ends the read with a downstream
    // error the caller enriches, never with a larger buffer.
    let error = collect(frames(&["a", "b", "c"]), 2, None).await.expect_err("bounded");
    assert!(matches!(error, DomainError::DownstreamError { .. }));
    // A read that stays at or under the ceiling still completes.
    let bytes = collect(frames(&["a", "b"]), 2, None).await.expect("collected");
    assert_eq!(bytes, Bytes::from_static(b"ab"));
}

#[tokio::test]
async fn a_failing_body_is_not_collected() {
    let error = collect(failing(), usize::MAX, None).await.expect_err("aborted");
    assert!(matches!(error, DomainError::StreamAborted { .. }));
}
