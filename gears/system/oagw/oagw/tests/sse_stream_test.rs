//! Server-Sent Events integration tests: a stream relayed byte for byte, an
//! upstream that cuts the stream short, and a client that hangs up.
//!
//! The upstream is a raw TCP server rather than a mock framework: a streamed
//! answer needs exact control over when the connection closes, which is the
//! behaviour under test.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::Harness;
use http_body_util::BodyExt;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The events the upstream sends when it does not abort.
const FULL_STREAM: [&str; 3] = ["data: first\n\n", "data: second\n\n", "data: third\n\n"];

/// How the upstream ends its answer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Every event, then a clean close.
    Complete,
    /// One event, then a close in the middle of the body.
    Truncated,
}

/// A streaming upstream answering on `127.0.0.1:{port}`.
struct StreamingUpstream {
    host: String,
    port: u16,
}

/// Frames a body as one HTTP chunk.
fn chunk(body: &str) -> String {
    format!("{:x}\r\n{body}\r\n", body.len())
}

/// The answer the upstream writes before it closes.
fn streamed_answer(ending: Ending) -> String {
    let mut answer = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
    );
    match ending {
        Ending::Complete => {
            for event in FULL_STREAM {
                answer.push_str(&chunk(event));
            }
            answer.push_str("0\r\n\r\n");
        }
        Ending::Truncated => answer.push_str(&chunk(FULL_STREAM[0])),
    }
    answer
}

/// Starts a one-shot streaming upstream with the given ending.
async fn spawn_upstream(ending: Ending) -> StreamingUpstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the upstream binds");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        // A failed answer ends the scenario; the client's read loop notices the
        // connection close and the test fails on its own terms.
        let served = answer_one(listener, ending).await;
        if served.is_err() {
            tracing::debug!("the streaming upstream never answered");
        }
    });
    StreamingUpstream {
        host: "127.0.0.1".to_owned(),
        port,
    }
}

/// Answers a single connection with the given ending.
async fn answer_one(listener: tokio::net::TcpListener, ending: Ending) -> std::io::Result<()> {
    let (mut socket, _) = listener.accept().await?;
    let mut head = String::new();
    let mut buffer = [0_u8; 2048];
    loop {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                head.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if head.contains("\r\n\r\n") {
                    break;
                }
            }
        }
    }
    socket.write_all(streamed_answer(ending).as_bytes()).await?;
    socket.flush().await?;
    socket.shutdown().await?;
    Ok(())
}

/// Registers an upstream and a streaming route, returning the upstream id.
async fn seeded(harness: &Harness, upstream: &StreamingUpstream) -> String {
    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "stream.example.com",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [
                    {"scheme": "http", "host": upstream.host, "port": upstream.port}
                ]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = harness
        .json(
            "POST",
            &format!("/oagw/v1/upstreams/{id}/routes"),
            Some(json!({
                "match": {"http": {"path": "/v1/events", "methods": ["GET"]}}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    id
}

/// What the client saw: status, headers and the events as they arrived.
///
/// `patience` bounds how long the client waits for the next frame; a stream
/// that never ends is dropped when it expires, which is how a client hangs up.
async fn read_stream(
    harness: &Harness,
    alias: &str,
    patience: Duration,
) -> (StatusCode, axum::http::HeaderMap, Vec<String>) {
    let response = harness
        .send(common::raw_request(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/v1/events"),
            &[],
            None,
        ))
        .await;
    let status = response.status();
    let headers = response.headers().clone();
    let mut events = Vec::new();
    let mut body = response.into_body();
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(patience, body.frame()).await {
        if let Ok(data) = frame.into_data() {
            events.push(String::from_utf8_lossy(&data).into_owned());
        }
    }
    (status, headers, events)
}

#[tokio::test]
async fn a_multi_event_stream_is_relayed_in_order_with_the_upstream_content_type() {
    let upstream = spawn_upstream(Ending::Complete).await;
    let harness = Harness::without_mock();
    seeded(&harness, &upstream).await;

    let (status, headers, events) =
        read_stream(&harness, "stream.example.com", Duration::from_secs(5)).await;

    assert_eq!(status, StatusCode::OK, "the upstream answered");
    assert_eq!(
        headers
            .get("content-type")
            .map(|value| value.to_str().unwrap_or_default()),
        Some("text/event-stream"),
        "the upstream content type is preserved"
    );
    assert_eq!(
        headers
            .get("cache-control")
            .map(|value| value.to_str().unwrap_or_default()),
        Some("no-cache"),
        "intermediaries are told not to buffer"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .map(|value| value.to_str().unwrap_or_default()),
        Some("upstream"),
        "the bytes came from the upstream"
    );
    assert_eq!(
        events.join(""),
        FULL_STREAM.join(""),
        "every event, in order"
    );
    assert_eq!(
        harness.metrics.snapshot().streams_relayed,
        1,
        "the relay is accounted for"
    );
}

#[tokio::test]
async fn an_upstream_that_cuts_the_stream_short_ends_the_client_stream() {
    let upstream = spawn_upstream(Ending::Truncated).await;
    let harness = Harness::without_mock();
    seeded(&harness, &upstream).await;

    let (status, _, events) =
        read_stream(&harness, "stream.example.com", Duration::from_secs(5)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        events.join(""),
        FULL_STREAM[0],
        "the client saw the one event the upstream completed"
    );
    assert!(
        harness.metrics.snapshot().streams_aborted >= 1,
        "the gateway accounted for the aborted stream"
    );
}

#[tokio::test]
async fn a_client_disconnect_is_propagated_to_the_upstream_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the upstream binds");
    let port = listener.local_addr().expect("address").port();
    let upstream = StreamingUpstream {
        host: "127.0.0.1".to_owned(),
        port,
    };
    let harness = Harness::without_mock();
    seeded(&harness, &upstream).await;

    // The upstream answers the handshake with a stream that never ends and
    // then waits for the client to hang up, which is what the test asserts.
    let (closed, seen_closed) = tokio::sync::oneshot::channel::<()>();
    let watcher = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("one connection");
        let written = socket
            .write_all(streamed_answer(Ending::Truncated).as_bytes())
            .await;
        let flushed = socket.flush().await;
        let mut buffer = [0_u8; 2048];
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let signalled = closed
            .send(())
            .map_err(|()| std::io::Error::other("the watcher's receiver was gone"));
        written.and(flushed).and(signalled)
    });

    // The client gives up on the never-ending stream and drops it.
    let (status, _, _) =
        read_stream(&harness, "stream.example.com", Duration::from_millis(700)).await;
    assert_eq!(status, StatusCode::OK);

    tokio::time::timeout(Duration::from_secs(5), seen_closed)
        .await
        .expect("the client's disconnect closed the upstream connection")
        .expect("the watcher finished");
    watcher
        .await
        .expect("the watcher task ends")
        .expect("the hang-up reached the upstream");
}
