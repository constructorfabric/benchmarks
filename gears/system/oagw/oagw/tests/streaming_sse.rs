//! Server-sent events and other streamed bodies travel as streams.
//!
//! A gateway that buffers makes streaming upstreams unusable, so these tests
//! hold the response under the caller's hands: what matters is that bytes
//! arrive while the upstream is still producing them, and that the connection
//! ends when the upstream ends.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use futures_util::StreamExt;
use http_body_util::BodyExt;

const SSE: &str = "text/event-stream";

/// A route to `upstream` that forwards the request path verbatim.
async fn wired(app: &common::TestApp, upstream: &LocalUpstream, prefix: &str) -> String {
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(serde_json::json!({
        "path": prefix,
        "methods": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    alias
}

/// The proxied body as a live stream.
async fn proxied(
    app: &common::TestApp,
    alias: &str,
    path: &str,
) -> http::Response<axum::body::Body> {
    app.send(app.request(
        http::Method::GET,
        &format!("/oagw/v1/proxy/{alias}{path}"),
        None,
        &[],
    ))
    .await
}

#[tokio::test]
async fn a_server_sent_event_stream_is_forwarded_incrementally() {
    let app = app().await;
    let chunks: Vec<Vec<u8>> = ["one", "two", "three"]
        .iter()
        .map(|name| format!("event: message\ndata: {name}\n\n").into_bytes())
        .collect();
    let upstream = LocalUpstream::start_with(common::Answer::stream(SSE, chunks.clone())).await;
    let alias = wired(&app, &upstream, "/v1/events").await;

    let response = proxied(&app, &alias, "/v1/events/stream").await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_default();
    assert_eq!(content_type, SSE, "the stream's media type is preserved");

    let mut stream = response.into_body().into_data_stream();
    for expected in chunks {
        let chunk = stream
            .next()
            .await
            .expect("the stream is still open")
            .expect("the chunk is readable");
        assert!(
            std::str::from_utf8(&expected).is_ok_and(|text| text.ends_with("\n\n")),
            "every event frame is well formed"
        );
        assert!(
            !chunk.is_empty(),
            "the caller receives a frame while the upstream is still producing"
        );
    }
    assert!(
        stream.next().await.is_none(),
        "the stream ends when the upstream ends"
    );
}

/// A streamed body of an arbitrary media type is not buffered either.
#[tokio::test]
async fn a_streamed_body_arrives_chunk_by_chunk() {
    let app = app().await;
    let upstream = LocalUpstream::start_with(common::Answer::stream(
        "application/x-ndjson",
        vec![b"{\"n\":1}\n".to_vec(), b"{\"n\":2}\n".to_vec()],
    ))
    .await;
    let alias = wired(&app, &upstream, "/v1/ndjson").await;

    let response = proxied(&app, &alias, "/v1/ndjson/feed").await;
    assert_eq!(response.status(), http::StatusCode::OK);

    let mut stream = response.into_body().into_data_stream();
    let first = stream
        .next()
        .await
        .expect("the first chunk arrives before the stream ends")
        .expect("the chunk decodes");
    assert_eq!(first_line(&first), "{\"n\":1}");

    let second = stream
        .next()
        .await
        .expect("the second chunk arrives")
        .expect("the chunk decodes");
    assert_eq!(first_line(&second), "{\"n\":2}");
    assert!(stream.next().await.is_none(), "the body ends cleanly");
}

/// The gateway does not invent a length it cannot know: a stream with no
/// declared length stays chunked rather than being rewritten.
#[tokio::test]
async fn a_stream_keeps_its_transfer_framing() {
    let app = app().await;
    let upstream = LocalUpstream::start_with(common::Answer::stream(
        SSE,
        vec![b"data: x\n\n".to_vec(), b"data: y\n\n".to_vec()],
    ))
    .await;
    let alias = wired(&app, &upstream, "/v1/framed").await;

    let response = proxied(&app, &alias, "/v1/framed/x").await;
    let framing = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .map(|value| value.to_str().unwrap_or_default().to_owned());
    assert_eq!(
        framing, None,
        "a streamed body declares no length it cannot know"
    );
}

/// A non-streaming answer still carries its whole body.
#[tokio::test]
async fn a_small_body_is_delivered_complete() {
    let app = app().await;
    let upstream = LocalUpstream::start_with(common::Answer::json(&serde_json::json!({
        "complete": true
    })))
    .await;
    let alias = wired(&app, &upstream, "/v1/small").await;

    let response = proxied(&app, &alias, "/v1/small/x").await;
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body reads")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(document["complete"], true);
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}
