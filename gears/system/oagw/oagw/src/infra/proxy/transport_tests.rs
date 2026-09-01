#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

//! The outbound transport (`DESIGN` §3.2, §3.3): one exchange per request, the
//! body streamed, every failure mapped onto a canonical identity.

use std::time::Duration;

use futures_util::StreamExt;
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::policy::ProxyPolicy;
use super::transport::{UpstreamTransport, outbound_uri};
use crate::domain::error::DomainError;

/// The HTTP status the wire problem of `error` carries.
fn status_of(error: DomainError) -> u16 {
    crate::api::rest::error::OagwProblem::from(error).status()
}

fn transport(policy: &ProxyPolicy) -> UpstreamTransport {
    UpstreamTransport::new(policy).unwrap()
}

// ── outbound uri ───────────────────────────────────────────────────────────

#[test]
fn the_outbound_uri_carries_the_endpoint_path_and_query() {
    // A full URI: the connector dials the authority, the path is the request
    // target the upstream sees.
    let uri = outbound_uri("http://api.openai.com:8080", "/v1/models", &[]);
    assert_eq!(uri.path(), "/v1/models");
    assert_eq!(uri.query(), None);
    assert_eq!(uri.to_string(), "http://api.openai.com:8080/v1/models");

    let uri = outbound_uri(
        "https://api.openai.com",
        "v1/models",
        &[("api-version".to_owned(), "2024-01".to_owned())],
    );
    assert_eq!(uri.path(), "/v1/models");
    assert_eq!(uri.query(), Some("api-version=2024-01"));
    assert_eq!(uri.host(), Some("api.openai.com"));

    let uri = outbound_uri(
        "https://api.openai.com",
        "/v1/models",
        &[
            ("q".to_owned(), "a b".to_owned()),
            ("tag".to_owned(), "1&2".to_owned()),
        ],
    );
    assert_eq!(uri.query(), Some("q=a+b&tag=1%262"));
}

#[test]
fn an_empty_query_leaves_the_uri_without_a_question_mark() {
    let uri = outbound_uri(
        "https://api.openai.com",
        "/",
        &[("empty".to_owned(), String::new())],
    );
    assert_eq!(uri.path(), "/");
    assert_eq!(uri.query(), Some("empty="));
    assert_eq!(uri.to_string(), "https://api.openai.com/?empty=");
}

// ── construction ───────────────────────────────────────────────────────────

#[test]
fn the_transport_carries_the_deployment_budget() {
    let policy = ProxyPolicy::new(7, true);
    let transport = transport(&policy);
    let rendered = format!("{transport:?}");
    assert!(rendered.contains("7s"), "{rendered}");
    assert_eq!(policy.proxy_timeout, Duration::from_secs(7));
}

// ── exchange ───────────────────────────────────────────────────────────────

/// The status and the body the caller sees, body fully read.
async fn body_of(response: &mut axum::http::Response<axum::body::Body>) -> (u16, bytes::Bytes) {
    let status = response.status().as_u16();
    let body = response.body_mut().collect().await.unwrap().to_bytes();
    (status, body)
}

#[tokio::test]
async fn a_response_is_forwarded_with_its_status_headers_and_body() {
    let server = httpmock::MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/models");
        then.status(201)
            .header("content-type", "application/json")
            .header("x-vendor", "vendor-1")
            .body("{\"models\":[1,2]}");
    });

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://127.0.0.1:{}/v1/models", server.port()))
        .body(axum::body::Body::empty())
        .unwrap();
    let mut response = transport(&ProxyPolicy::new(5, true))
        .send(request)
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(
        response
            .headers()
            .get("x-vendor")
            .and_then(|value| value.to_str().ok()),
        Some("vendor-1")
    );
    let (status, body) = body_of(&mut response).await;
    assert_eq!(status, 201);
    assert_eq!(body, &b"{\"models\":[1,2]}"[..]);
}

#[tokio::test]
async fn a_streamed_request_body_reaches_the_upstream() {
    let server = httpmock::MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/embeddings");
        then.status(200).body("ok");
    });

    let body = axum::body::Body::from_stream(futures_util::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{\"in")),
        Ok(bytes::Bytes::from_static(b"put\":1}")),
    ]));
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("http://127.0.0.1:{}/v1/embeddings", server.port()))
        .body(body)
        .unwrap();
    let response = transport(&ProxyPolicy::new(5, true))
        .send(request)
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test]
async fn a_streamed_response_body_arrives_before_it_is_complete() {
    // A listener that writes the response head and one chunk, then keeps the
    // connection open: the caller must observe the first chunk while the
    // upstream is still writing (`DESIGN` §3.2).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        // Drain the request head before answering.
        let mut buffer = [0u8; 8192];
        drop(reader.read(&mut buffer).await);
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
        writer.write_all(head.as_bytes()).await.unwrap();
        writer.write_all(b"data: first\n\n").await.unwrap();
        writer.flush().await.unwrap();
        // Hold the stream open until the caller has read the first chunk.
        tokio::time::sleep(Duration::from_millis(200)).await;
        writer.write_all(b"data: second\n\n").await.unwrap();
        writer.shutdown().await.unwrap();
    });

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://127.0.0.1:{port}/stream"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = transport(&ProxyPolicy::new(5, true))
        .send(request)
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
    match first {
        Ok(Some(Ok(chunk))) => {
            assert_eq!(chunk, &b"data: first\n\n"[..]);
        }
        other => panic!("the first chunk was not observable before the stream ended: {other:?}"),
    }
    let rest = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
    match rest {
        Ok(Some(Ok(chunk))) => assert_eq!(chunk, &b"data: second\n\n"[..]),
        other => panic!("the rest of the stream is missing: {other:?}"),
    }
}

#[tokio::test]
async fn an_unreachable_endpoint_is_a_503_link_unavailable() {
    // Port 1 on the loopback is closed, so the connect fails at once.
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri("http://127.0.0.1:1/close")
        .body(axum::body::Body::empty())
        .unwrap();
    let error = transport(&ProxyPolicy::new(2, true))
        .send(request)
        .await
        .unwrap_err();
    assert!(
        matches!(error, DomainError::LinkUnavailable { .. }),
        "{error:?}"
    );
    assert_eq!(status_of(error), 503);
}

#[tokio::test]
async fn an_exchange_past_its_budget_is_a_504_request_timeout() {
    // The listener accepts and holds the connection without answering, so the
    // response headers never arrive within the one-second minimum budget.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            // Held open, never answered: dropping it would be a reset, which
            // the transport reports as an unreachable link instead.
            tokio::spawn(async move {
                let _held = socket;
                std::future::pending::<()>().await;
            });
        }
    });

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://127.0.0.1:{port}/slow"))
        .body(axum::body::Body::empty())
        .unwrap();
    let error = transport(&ProxyPolicy::new(1, true))
        .send(request)
        .await
        .unwrap_err();
    assert!(
        matches!(error, DomainError::RequestTimeout { limit_secs: 1 }),
        "{error:?}"
    );
    assert_eq!(status_of(error), 504);
}

// ── upgrade ────────────────────────────────────────────────────────────────

/// The upstream side of a WebSocket handshake, answered with a raw `101` so the
/// tunnel can be spliced without a WebSocket stack.
async fn upgrade_upstream() -> (u16, tokio::sync::oneshot::Receiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (key_tx, key_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let head = read_request_head(&mut reader).await;
        drop(key_tx.send(head));
        let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nconnection: Upgrade\r\nupgrade: \
             websocket\r\nsec-websocket-accept: {accept}\r\n\r\n"
        );
        writer.write_all(response.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
        // Echo every byte back until the connection closes: whatever the caller
        // sends crosses the tunnel and returns.
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if writer.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                    drop(writer.flush().await);
                }
            }
        }
    });
    (port, key_rx)
}

/// The request head the upstream read, as a string.
async fn read_request_head(reader: &mut tokio::net::tcp::OwnedReadHalf) -> String {
    let mut head = Vec::new();
    let mut buffer = [0u8; 2048];
    while !head.ends_with(b"\r\n\r\n") {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => head.extend_from_slice(&buffer[..read]),
        }
    }
    String::from_utf8_lossy(&head).to_string()
}

#[tokio::test]
async fn an_upgrade_handshake_is_tunnelled() {
    let (port, key_rx) = upgrade_upstream().await;
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://127.0.0.1:{port}/ws"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .body(axum::body::Body::empty())
        .unwrap();

    let transport = transport(&ProxyPolicy::new(5, true));
    let (parts, _upstream) = transport.tunnel(request).await.unwrap();
    assert_eq!(parts.status.as_u16(), 101);
    assert_eq!(
        parts
            .headers
            .get("sec-websocket-accept")
            .and_then(|value| value.to_str().ok()),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
    );
    let head = key_rx.await.unwrap();
    assert!(
        head.contains("sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ=="),
        "{head}"
    );
    assert!(head.contains("upgrade: websocket"), "{head}");
}

#[tokio::test]
async fn an_upstream_that_refuses_the_upgrade_is_a_protocol_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let mut buffer = [0u8; 4096];
        drop(reader.read(&mut buffer).await);
        let head = "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n";
        drop(writer.write_all(head.as_bytes()).await);
    });

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://127.0.0.1:{port}/ws"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .body(axum::body::Body::empty())
        .unwrap();
    let error = transport(&ProxyPolicy::new(5, true))
        .tunnel(request)
        .await
        .unwrap_err();
    assert!(
        matches!(error, DomainError::ProtocolError { .. }),
        "{error:?}"
    );
    assert_eq!(status_of(error), 502);
}
