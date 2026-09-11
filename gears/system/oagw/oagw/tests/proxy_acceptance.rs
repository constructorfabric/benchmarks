//! Acceptance tests for the data plane.
//!
//! These codify the section 6 acceptance criteria of `proxy-http.md`,
//! `proxy-streaming.md` and `traffic-policy.md` against a real upstream.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use axum::http::StatusCode;
use common::{FakeUpstream, FakeWsUpstream, Harness, header};

const SOURCE: &str = "x-oagw-error-source";

// ---- plain HTTP proxying ----------------------------------------------

#[tokio::test]
async fn a_proxied_get_is_relayed_and_marked_upstream() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("svc", "http", up.port).await;

    let (s, hdrs, body) = h.raw("GET", "/oagw/v1/proxy/svc/hello", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("upstream"));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["path"], "/hello");
}

#[tokio::test]
async fn the_path_suffix_and_query_reach_the_upstream() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("svc", "http", up.port).await;

    let (s, _, body) = h
        .raw("GET", "/oagw/v1/proxy/svc/a/b/c?x=1&y=2", None, &[])
        .await;
    assert_eq!(s, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["path"], "/a/b/c?x=1&y=2");
}

#[tokio::test]
async fn the_host_header_is_replaced_with_the_upstream_authority() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("svc", "http", up.port).await;

    let (_, _, body) = h
        .raw("GET", "/oagw/v1/proxy/svc/x", None, &[("host", "gateway.example")])
        .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["host"], format!("127.0.0.1:{}", up.port));
}

#[tokio::test]
async fn hop_by_hop_and_routing_headers_are_not_forwarded() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("svc", "http", up.port).await;

    let (_, _, body) = h
        .raw(
            "GET",
            "/oagw/v1/proxy/svc/x",
            None,
            &[("te", "trailers"), ("x-oagw-target-host", "127.0.0.1")],
        )
        .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["saw_hop_by_hop_te"], false, "TE must be stripped");
    assert_eq!(
        v["saw_target_host"], false,
        "the routing header must be consumed, not forwarded"
    );
}

#[tokio::test]
async fn an_upstream_5xx_is_relayed_unchanged_and_marked_upstream() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("svc", "http", up.port).await;

    let (s, hdrs, body) = h.raw("GET", "/oagw/v1/proxy/svc/boom", None, &[]).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("upstream"));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["upstream"], "error");
}

#[tokio::test]
async fn an_unknown_alias_is_404_marked_gateway() {
    let h = Harness::graded();
    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/nope/x", None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn a_disabled_upstream_answers_503() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "off",
                "enabled": false,
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": up.port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/"}}
        })),
    )
    .await;

    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/off/x", None, &[]).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn a_request_matching_no_route_is_404_marked_gateway() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "narrow",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": up.port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/allowed"}}
        })),
    )
    .await;

    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/narrow/elsewhere", None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn an_upstream_slower_than_the_timeout_is_504() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded(); // proxy_timeout_secs = 2
    h.wire_upstream("svc", "http", up.port).await;

    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/svc/slow", None, &[]).await;
    assert_eq!(s, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn an_unreachable_upstream_is_502() {
    let h = Harness::graded();
    // Port 1 on loopback has nothing listening.
    h.wire_upstream("dead", "http", 1).await;
    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/dead/x", None, &[]).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn a_plaintext_connection_is_refused_when_the_flag_is_off() {
    let up = FakeUpstream::start().await;
    // The upstream is still *created* successfully with an http scheme — only
    // the connection is refused.
    let h = Harness::plaintext_refused();
    h.wire_upstream("svc", "http", up.port).await;

    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/svc/x", None, &[]).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn a_plaintext_connection_is_made_when_the_flag_is_on() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded(); // allow_http_upstream = true
    h.wire_upstream("svc", "http", up.port).await;
    let (s, _, _) = h.raw("GET", "/oagw/v1/proxy/svc/x", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn a_target_host_naming_an_endpoint_outside_the_pool_is_400() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("svc", "http", up.port).await;

    let (s, hdrs, _) = h
        .raw(
            "GET",
            "/oagw/v1/proxy/svc/x",
            None,
            &[("x-oagw-target-host", "elsewhere.example")],
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn a_target_host_naming_the_sole_endpoint_is_accepted() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("svc", "http", up.port).await;

    let (s, _, _) = h
        .raw(
            "GET",
            "/oagw/v1/proxy/svc/x",
            None,
            &[("x-oagw-target-host", "127.0.0.1")],
        )
        .await;
    assert_eq!(s, StatusCode::OK);
}

// ---- server-sent events ------------------------------------------------

#[tokio::test]
async fn an_event_stream_is_relayed_in_order_and_outlives_the_proxy_timeout() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded(); // proxy_timeout_secs = 2
    h.wire_upstream("svc", "http", up.port).await;

    let started = std::time::Instant::now();
    let (s, hdrs, body) = h.raw("GET", "/oagw/v1/proxy/svc/sse", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        header(&hdrs, "content-type").as_deref(),
        Some("text/event-stream")
    );
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains("data: ev0"), "got: {text}");
    assert!(text.contains("data: ev1"), "got: {text}");
    assert!(text.contains("data: ev2"), "got: {text}");
    // Events are in order.
    let (i0, i1, i2) = (
        text.find("ev0").unwrap(),
        text.find("ev1").unwrap(),
        text.find("ev2").unwrap(),
    );
    assert!(i0 < i1 && i1 < i2, "events out of order: {text}");
    // The stream ran past the 2 s timeout without being cut off.
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(3),
        "the stream should have outlived proxy_timeout_secs"
    );
}

#[tokio::test]
async fn a_stream_carries_the_upstream_error_source() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("svc", "http", up.port).await;
    let (_, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/svc/sse", None, &[]).await;
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("upstream"));
}

// ---- WebSocket ---------------------------------------------------------

#[tokio::test]
async fn a_websocket_upgrade_is_relayed_with_frames_and_close_code() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let ws = FakeWsUpstream::start().await;

    // The upgrade needs a real listening socket, so the gear's router is served
    // on a loopback port rather than driven through `oneshot`.
    let h = Harness::graded();
    h.wire_upstream("chat", "ws", ws.port).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    let app = h.router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", gw_port))
        .await
        .unwrap();
    let req = "GET /oagw/v1/proxy/chat/room HTTP/1.1\r\n\
               Host: 127.0.0.1\r\n\
               Upgrade: websocket\r\n\
               Connection: Upgrade\r\n\
               Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
               Sec-WebSocket-Version: 13\r\n\
               Sec-WebSocket-Protocol: chat.v1\r\n\r\n";
    sock.write_all(req.as_bytes()).await.unwrap();

    // Read the handshake response.
    let mut buf = vec![0_u8; 4096];
    let n = sock.read(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "expected a 101, got: {head}"
    );
    assert!(
        head.to_lowercase().contains("sec-websocket-accept:"),
        "handshake missing accept: {head}"
    );
    assert!(
        head.to_lowercase().contains("chat.v1"),
        "the negotiated subprotocol should be relayed: {head}"
    );

    // Send a masked text frame and read the echo.
    let payload = b"hello";
    let mask = [0x01_u8, 0x02, 0x03, 0x04];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    sock.write_all(&frame).await.unwrap();

    let n = sock.read(&mut buf).await.unwrap();
    assert!(n >= 2, "expected an echoed frame");
    let len = usize::from(buf[1] & 0x7f);
    let echoed = String::from_utf8_lossy(&buf[2..2 + len]).to_string();
    assert_eq!(echoed, "ECHO:hello");

    // Close with code 1000 and check the close frame comes back.
    let close_payload = [0x03_u8, 0xe8]; // 1000
    let mut close = vec![0x88, 0x80 | close_payload.len() as u8];
    close.extend_from_slice(&mask);
    close.extend(close_payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    sock.write_all(&close).await.unwrap();

    let n = sock.read(&mut buf).await.unwrap();
    assert!(n >= 4, "expected a close frame back");
    assert_eq!(buf[0] & 0x0f, 0x8, "expected a close opcode");
    assert_eq!(
        u16::from_be_bytes([buf[2], buf[3]]),
        1000,
        "the close code should propagate"
    );
}

// ---- CORS --------------------------------------------------------------

#[tokio::test]
async fn a_preflight_is_answered_204_without_touching_the_upstream() {
    let h = Harness::graded();
    // Deliberately no upstream registered: the preflight must be answered
    // before upstream resolution.
    let (s, hdrs, _) = h
        .raw(
            "OPTIONS",
            "/oagw/v1/proxy/whatever/x",
            None,
            &[
                ("origin", "https://app.example"),
                ("access-control-request-method", "PUT"),
                ("access-control-request-headers", "x-a"),
            ],
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(
        header(&hdrs, "access-control-allow-origin").as_deref(),
        Some("https://app.example")
    );
    assert_eq!(
        header(&hdrs, "access-control-allow-methods").as_deref(),
        Some("PUT")
    );
    assert_eq!(
        header(&hdrs, "access-control-max-age").as_deref(),
        Some("86400")
    );
    assert!(header(&hdrs, "vary").is_some());
}

#[tokio::test]
async fn a_disallowed_origin_on_an_actual_request_is_403() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "corsed",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": up.port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "cors": {"enabled": true, "allowed_origins": ["https://good.example"], "allowed_methods": ["GET"]}
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/"}}
        })),
    )
    .await;

    let (s, hdrs, body) = h
        .raw(
            "GET",
            "/oagw/v1/proxy/corsed/x",
            None,
            &[("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["type"], "cf.oagw.cors.origin_not_allowed.v1");

    // An allowed origin passes and gains the CORS response headers.
    let (s, hdrs, _) = h
        .raw(
            "GET",
            "/oagw/v1/proxy/corsed/x",
            None,
            &[("origin", "https://good.example")],
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        header(&hdrs, "access-control-allow-origin").as_deref(),
        Some("https://good.example")
    );
}

#[tokio::test]
async fn a_disallowed_method_on_an_actual_request_is_403_with_its_own_type() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": "corsm",
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": up.port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "cors": {"enabled": true, "allowed_origins": ["https://good.example"], "allowed_methods": ["GET"]}
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET", "DELETE"], "path": "/"}}
        })),
    )
    .await;

    let (s, _, body) = h
        .raw(
            "DELETE",
            "/oagw/v1/proxy/corsm/x",
            None,
            &[("origin", "https://good.example")],
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["type"], "cf.oagw.cors.method_not_allowed.v1");
}

// ---- rate limiting -----------------------------------------------------

async fn wire_rate_limited(h: &Harness, port: u16, alias: &str, rate: u32) {
    let (_, created) = h
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(serde_json::json!({
                "alias": alias,
                "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": port}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "rate_limit": {"sustained": {"rate": rate, "window": "hour"}, "burst": {"capacity": rate}}
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    h.json(
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({
            "upstream_id": id,
            "match": {"http": {"methods": ["GET"], "path": "/"}}
        })),
    )
    .await;
}

#[tokio::test]
async fn an_allowed_request_carries_the_rate_limit_headers() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    wire_rate_limited(&h, up.port, "rl1", 5).await;

    let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/rl1/x", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(header(&hdrs, "x-ratelimit-limit").as_deref(), Some("5"));
    assert_eq!(header(&hdrs, "x-ratelimit-remaining").as_deref(), Some("4"));
    assert!(header(&hdrs, "x-ratelimit-reset").is_some());
}

#[tokio::test]
async fn exceeding_the_rate_limit_answers_429_with_retry_after() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    // One token per hour: the second request cannot be admitted.
    wire_rate_limited(&h, up.port, "rl2", 1).await;

    let (s1, _, _) = h.raw("GET", "/oagw/v1/proxy/rl2/x", None, &[]).await;
    assert_eq!(s1, StatusCode::OK);

    let (s2, hdrs, body) = h.raw("GET", "/oagw/v1/proxy/rl2/x", None, &[]).await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&hdrs, SOURCE).as_deref(), Some("gateway"));
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(
        text.contains("retry_after") || header(&hdrs, "retry-after").is_some(),
        "a rejection should carry a retry hint: {text}"
    );
}

#[tokio::test]
async fn an_upstream_without_a_rate_limit_is_not_limited() {
    let up = FakeUpstream::start().await;
    let h = Harness::graded();
    h.wire_upstream("free", "http", up.port).await;
    for _ in 0..5 {
        let (s, hdrs, _) = h.raw("GET", "/oagw/v1/proxy/free/x", None, &[]).await;
        assert_eq!(s, StatusCode::OK);
        assert!(header(&hdrs, "x-ratelimit-limit").is_none());
    }
}
