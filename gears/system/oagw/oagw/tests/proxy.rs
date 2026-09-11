// Created: 2026-09-02 by Constructor Tech
//! Data-plane tests: alias resolution, route matching, validation, and the
//! three relay shapes the specification requires — plain HTTP, server-sent
//! events and a WebSocket upgrade.
//!
//! A real axum server is spawned as the upstream, so what is asserted is the
//! behaviour on the wire, including the streaming and upgrade paths that
//! `oneshot` cannot express.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{Request, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use support::{TENANT_A, TENANT_B, default_config, gateway, serve};
use tower::ServiceExt;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// What the test upstream saw, echoed back as JSON.
async fn echo(request: Request<Body>) -> Response {
    let path = request.uri().path().to_owned();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&json!({
            "path": path,
            "headers": headers,
            "body": String::from_utf8_lossy(&body).into_owned(),
        }))
        .unwrap(),
    )
        .into_response()
}

/// An endless server-sent-event stream, cut short by the client hanging up.
async fn stream() -> impl IntoResponse {
    let events = futures_util::stream::unfold(0u32, |n| async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        Some((Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().data(format!("tick-{n}"))), n + 1))
    });
    axum::response::Sse::new(events).keep_alive(axum::response::sse::KeepAlive::default())
}

/// A WebSocket echo endpoint, for the upgrade path.
async fn socket(State(seen): State<Arc<AtomicUsize>>, upgrade: WebSocketUpgrade) -> Response {
    eprintln!("DBG upstream: upgrade requested");
    upgrade.on_upgrade(move |mut ws: WebSocket| async move {
        eprintln!("DBG upstream: socket acquired");
        loop {
            match ws.recv().await {
                Some(Ok(message)) => {
                    seen.fetch_add(1, Ordering::Relaxed);
                    eprintln!("DBG upstream: got {message:?}");
                    let Message::Text(text) = message else { continue };
                    if ws.send(Message::Text(format!("echo:{text}").into())).await.is_err() {
                        break;
                    }
                }
                Some(Err(e)) => {
                    eprintln!("DBG upstream: recv error {e}");
                    break;
                }
                None => {
                    eprintln!("DBG upstream: recv None");
                    break;
                }
            }
        }
        eprintln!("DBG upstream: handler done");
    })
}

/// Spawns the upstream and returns its base URL plus its "messages seen" counter.
async fn upstream() -> (String, Arc<AtomicUsize>) {
    let seen = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/", axum::routing::any(echo))
        .route("/{*path}", axum::routing::any(echo))
        .route("/sse", axum::routing::get(stream))
        .route("/ws", axum::routing::get(socket))
        .with_state(seen.clone());
    (format!("http://{}", serve(app).await), seen)
}

/// Registers an upstream plus a catch-all route and returns its id.
async fn register(router: &Router, base: &str, alias: &str, path: &str) -> String {
    let (host, port) = base.trim_start_matches("http://").split_once(':').unwrap();
    let spec = json!({
        "alias": alias,
        "server": {"endpoints": [{"scheme": "http", "host": host, "port": port.parse::<u16>().unwrap()}]},
        "protocol": PROTOCOL,
    });
    let (status, upstream) = support::post_json(router, "POST", "/oagw/v1/upstreams", spec).await;
    assert_eq!(status, 201, "{upstream}");
    let (status, body) = support::post_json(
        router,
        "POST",
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream["id"],
            "match": {"http": {"methods": ["GET", "POST"], "path": path, "query_allowlist": ["a", "b"]}},
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    upstream["id"].as_str().unwrap().to_owned()
}

/// The streamed body as text, bounded so a non-terminating stream cannot hang.
async fn read_stream(response: axum::response::Response, events: usize) -> String {
    let mut stream = response.into_body().into_data_stream();
    let mut out = String::new();
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    out.push_str(&String::from_utf8_lossy(&bytes));
                    if out.matches("data:").count() >= events {
                        break;
                    }
                }
                _ => break,
            },
        }
    }
    out
}

// ------------------------------------------------------------------- plain HTTP

#[tokio::test]
async fn a_get_request_is_relayed_with_its_path_and_query() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "echo", "/").await;

    let response = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/echo/v1/things?a=1", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body = support::json_of(response).await;
    assert_eq!(body["path"], json!("/v1/things"));
    assert!(
        body["headers"].as_array().unwrap().iter().any(|h| h[0].as_str() == Some("x-request-id")),
        "{body}"
    );
}

#[tokio::test]
async fn the_caller_s_credentials_are_not_forwarded() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "echo", "/").await;

    let response = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/echo/v1", None, TENANT_A))
        .await
        .unwrap();
    let body = support::json_of(response).await;
    assert!(
        !body["headers"].as_array().unwrap().iter().any(|h| h[0].as_str() == Some("authorization")),
        "the inbound bearer token leaked upstream: {body}"
    );
}

#[tokio::test]
async fn a_post_body_reaches_the_upstream() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "echo", "/").await;

    let response = router
        .oneshot(support::test_request(
            "POST",
            "/oagw/v1/proxy/echo/create",
            Some(json!({"k": "v"})),
            TENANT_A,
        ))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body = support::json_of(response).await;
    assert_eq!(body["body"], json!(r#"{"k":"v"}"#), "{body}");
}

#[tokio::test]
async fn a_bare_alias_and_its_trailing_slash_reach_the_route_path() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "echo", "/").await;

    let bare = router
        .clone()
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/echo", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(bare.status().as_u16(), 200);
    assert_eq!(support::json_of(bare).await["path"], json!("/"));

    let slashed = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/echo/", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(slashed.status().as_u16(), 200);
    assert_eq!(support::json_of(slashed).await["path"], json!("/"));
}

#[tokio::test]
async fn an_unlisted_query_parameter_is_rejected() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "echo", "/").await;

    let allowed = router
        .clone()
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/echo/v1?a=1", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(allowed.status().as_u16(), 200);

    let rejected = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/echo/v1?c=3", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(rejected.status().as_u16(), 400);
}

// ------------------------------------------------------------------------ SSE

#[tokio::test]
async fn a_server_sent_event_stream_is_relayed_incrementally() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "stream", "/").await;

    let response = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/stream/sse", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "{:?}",
        response.headers()
    );

    let body = read_stream(response, 3).await;
    assert!(body.contains("data: tick-0"), "{body}");
    assert!(body.contains("data: tick-1"), "{body}");
}

// ------------------------------------------------------------------ WebSocket

/// Writes a masked client text frame and reads a server text frame back.
///
/// The splice hands over a raw byte stream, so the test speaks the WebSocket
/// framing itself: one masked frame out, one unmasked frame in.
mod frames {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Sends a masked, unfragmented text frame.
    pub async fn send_text<S: tokio::io::AsyncWrite + Unpin>(
        io: &mut S,
        text: &str,
    ) -> std::io::Result<()> {
        let payload = text.as_bytes();
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        let masked: Vec<u8> =
            payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect();
        let mut frame = vec![0x81u8];
        assert!(payload.len() < 126, "test frames stay in the short form");
        // A client frame is always masked: the length byte carries the mask bit.
        frame.push(0x80 | payload.len() as u8);
        frame.extend_from_slice(&mask);
        frame.extend_from_slice(&masked);
        io.write_all(&frame).await?;
        io.flush().await
    }

    /// Reads a text frame, ignoring the ping the peer may interleave.
    pub async fn recv_text<S: tokio::io::AsyncRead + Unpin>(io: &mut S) -> String {
        try_recv_text(io).await.expect("frame")
    }

    /// As `recv_text`, but `None` when the peer hangs up.
    pub async fn try_recv_text<S: tokio::io::AsyncRead + Unpin>(io: &mut S) -> Option<String> {
        loop {
            let mut head = [0u8; 2];
            io.read_exact(&mut head).await.ok()?;
            let opcode = head[0] & 0x0f;
            let length = usize::from(head[1] & 0x7f);
            let mut payload = vec![0u8; length];
            io.read_exact(&mut payload).await.ok()?;
            if opcode == 0x1 {
                return Some(String::from_utf8_lossy(&payload).into_owned());
            }
        }
    }
}

#[tokio::test]
async fn a_websocket_upgrade_is_spliced_to_the_upstream() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "chat", "/").await;
    let addr = serve(router).await;

    // An upgrade needs a real socket: `oneshot` never reaches hyper, so no
    // `OnUpgrade` extension exists for the relay to hand the caller. The
    // handshake is spoken by hand for the same reason — what is under test is
    // the splice, not a WebSocket client library.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut raw = tokio::net::TcpStream::connect(addr).await.unwrap();
    let handshake = format!(
        "GET /oagw/v1/proxy/chat/ws HTTP/1.1\r\nhost: {addr}\r\nconnection: Upgrade\r\nupgrade: websocket\r\nsec-websocket-version: 13\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nx-oagw-test-tenant: {TENANT_A}\r\n\r\n"
    );
    raw.write_all(handshake.as_bytes()).await.unwrap();
    raw.flush().await.unwrap();

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if raw.read(&mut byte).await.unwrap() == 0 {
            panic!("the gateway hung up before the handshake completed");
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(head.contains("upgrade: websocket"), "{head}");
    assert!(head.contains("connection: upgrade"), "{head}");
    // The accept digest is the upstream's answer to the key, relayed verbatim.
    assert!(
        head.contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "{head}"
    );

    let mut raw = tokio::io::BufStream::new(raw);
    frames::send_text(&mut raw, "hello gateway").await.unwrap();
    assert_eq!(frames::recv_text(&mut raw).await, "echo:hello gateway");
    frames::send_text(&mut raw, "again").await.unwrap();
    assert_eq!(frames::recv_text(&mut raw).await, "echo:again");
    assert_eq!(_seen.load(Ordering::Relaxed), 2, "both frames reached the upstream");
}

// --------------------------------------------------------------- error paths

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "echo", "/").await;

    let response = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/ghost/v1", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let problem_type = problem_type(response).await;
    assert!(problem_type.ends_with("not_found.v1"), "{problem_type}");
}

#[tokio::test]
async fn another_tenant_cannot_reach_an_upstream_by_alias() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "echo", "/").await;

    let response = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/echo/v1", None, TENANT_B))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503() {
    let (base, _seen) = upstream().await;
    let router = gateway(default_config()).await;
    register(&router, &base, "echo", "/").await;

    let (_, list) = support::post_json(&router, "GET", "/oagw/v1/upstreams", Value::Null).await;
    let id = list.as_array().unwrap()[0]["id"].as_str().unwrap().to_owned();
    let (_, got) = support::post_json(&router, "GET", &format!("/oagw/v1/upstreams/{id}"), Value::Null).await;
    let mut spec = got.clone();
    spec["enabled"] = json!(false);
    let (status, _) = support::post_json(&router, "PUT", &format!("/oagw/v1/upstreams/{id}"), spec).await;
    assert_eq!(status, 200);

    let response = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/echo/v1", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503, "{:?}", response.status());
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502() {
    let router = gateway(default_config()).await;
    let (_, upstream) = support::post_json(
        &router,
        "POST",
        "/oagw/v1/upstreams",
        json!({
            "alias": "dark",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 1}]},
            "protocol": PROTOCOL,
        }),
    )
    .await;
    let (status, _) = support::post_json(
        &router,
        "POST",
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream["id"],
            "match": {"http": {"methods": ["GET"], "path": "/", "query_allowlist": []}},
        }),
    )
    .await;
    assert_eq!(status, 201);

    let response = router
        .oneshot(support::test_request("GET", "/oagw/v1/proxy/dark/v1", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    let problem_type = problem_type(response).await;
    assert!(problem_type.ends_with("link.unavailable.v1"), "{problem_type}");
}

async fn problem_type(response: axum::response::Response) -> String {
    support::json_of(response)
        .await
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
