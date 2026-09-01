// Created: 2026-08-29 by Constructor Tech
//! Server-sent events relayed chunk by chunk, and the mid-stream abort frame.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{json_body, post, tenant};
use futures_util::StreamExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Serve exactly one raw HTTP/1.1 request with `handler`.
async fn serve_one<F, Fut>(handler: F) -> u16
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            handler(socket).await;
        }
    });
    port
}

/// Write one chunked-transfer body chunk.
async fn write_chunk(socket: &mut TcpStream, data: &[u8]) {
    socket
        .write_all(format!("{:x}\r\n", data.len()).as_bytes())
        .await
        .expect("chunk length");
    socket.write_all(data).await.expect("chunk body");
    socket.write_all(b"\r\n").await.expect("chunk terminator");
}

/// Status line plus the SSE headers, all through the chunked transfer encoding.
async fn write_sse_head(socket: &mut TcpStream) {
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ntransfer-encoding: chunked\r\n\r\n",
        )
        .await
        .expect("sse head");
    socket.flush().await.expect("flush sse head");
}

async fn register_upstream(harness: &common::Harness, alias: &str, port: u16) -> String {
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": alias,
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;
    id
}

/// Collect the relayed body frames with a hard deadline per frame.
async fn frames(response: axum::response::Response) -> Vec<bytes::Bytes> {
    let mut stream = response.into_body().into_data_stream();
    let mut collected = Vec::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(frame))) => collected.push(frame),
            Ok(Some(Err(error))) => panic!("relay failed: {error}"),
            Ok(None) | Err(_) => break,
        }
    }
    collected
}

#[tokio::test]
async fn sse_frames_are_relayed_as_they_arrive() {
    let harness = common::Harness::new(common::test_config(), None);
    let port = serve_one(|mut socket| async move {
        let mut buffer = [0u8; 4096];
        let _ = socket.read(&mut buffer).await;
        write_sse_head(&mut socket).await;
        for event in [
            "event: alpha\ndata: 1\n\n",
            "event: beta\ndata: 2\n\n",
            "event: gamma\ndata: 3\n\n",
        ] {
            write_chunk(&mut socket, event.as_bytes()).await;
            socket.flush().await.expect("flush event");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        socket.write_all(b"0\r\n\r\n").await.expect("end of stream");
        socket.flush().await.expect("flush end");
    })
    .await;
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "sse.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/sse.example.com/api")
        .header("accept", "text/event-stream")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        common::header(&response, "content-type").as_deref(),
        Some("text/event-stream")
    );
    // A relayed stream is produced by the upstream, so ADR-0007 attributes it to
    // the upstream even though it is a success.
    assert_eq!(
        common::header(&response, "x-oagw-error-source").as_deref(),
        Some("upstream")
    );

    let relayed = frames(response).await;
    assert!(relayed.len() >= 2, "events must be relayed chunk by chunk");
    let body = relayed
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk.as_ref()).to_string())
        .collect::<Vec<_>>()
        .join("");
    for expected in [
        "event: alpha\ndata: 1\n\n",
        "event: beta\ndata: 2\n\n",
        "event: gamma\ndata: 3\n\n",
    ] {
        assert!(body.contains(expected), "missing {expected:?} in {body:?}");
    }
    assert!(
        !body.contains("event: error"),
        "a complete stream must not error: {body:?}"
    );
}

#[tokio::test]
async fn a_mid_stream_abort_becomes_an_error_frame() {
    let harness = common::Harness::new(common::test_config(), None);
    let port = serve_one(|mut socket| async move {
        let mut buffer = [0u8; 4096];
        let _ = socket.read(&mut buffer).await;
        write_sse_head(&mut socket).await;
        write_chunk(&mut socket, b"event: alpha\ndata: 1\n\n").await;
        socket.flush().await.expect("flush event");
        // Drop the socket without terminating the chunked body: the upstream
        // stream aborts mid-flight.
        drop(socket);
    })
    .await;
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "abort.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": upstream_id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/abort.example.com/api")
        .header("accept", "text/event-stream")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 200, "headers are already sent");
    let relayed = frames(response).await;
    let body = relayed
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk.as_ref()).to_string())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        body.contains("event: alpha\ndata: 1\n\n"),
        "the first frame is intact: {body:?}"
    );
    assert!(
        body.contains(
            "event: error\ndata: gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1\n\n"
        ),
        "the abort must be reported as an error frame: {body:?}"
    );
    // No credential, body or header material is ever echoed into the frame.
    assert!(!body.contains("authorization"), "{body:?}");
}

#[tokio::test]
async fn an_upstream_sse_content_type_streams_even_without_an_accept_header() {
    let harness = common::Harness::new(common::test_config(), None);
    let port = serve_one(|mut socket| async move {
        let mut buffer = [0u8; 4096];
        let _ = socket.read(&mut buffer).await;
        write_sse_head(&mut socket).await;
        write_chunk(&mut socket, b"data: only\n\n").await;
        socket.write_all(b"0\r\n\r\n").await.expect("end of stream");
        socket.flush().await.expect("flush end");
    })
    .await;
    register_upstream(&harness, "no-accept.example.com", port).await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/no-accept.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    let relayed = frames(response).await;
    let body = relayed
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk.as_ref()).to_string())
        .collect::<Vec<_>>()
        .join("");
    assert!(body.contains("data: only\n\n"), "{body:?}");
}
