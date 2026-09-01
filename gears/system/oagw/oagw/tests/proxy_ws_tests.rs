// Data Plane integration tests: WebSocket upgrades.
//
// The gateway relays the upstream `101 Switching Protocols` verbatim and then
// splices raw bytes between the client and the upstream connection, so the
// test drives a real TCP handshake on both sides and asserts on the payload
// travelling in both directions after the upgrade.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::{
    Harness, ProxyOptions, context_for, echo_upgrade_upstream, echo_upstream, gateway_request,
    http_route, text, upstream_shell,
};
use tenant_resolver_sdk::TenantId;

fn options() -> ProxyOptions {
    ProxyOptions {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_enabled: false,
    }
}

#[tokio::test]
async fn an_upgrade_is_relayed_and_bytes_are_spliced() {
    let address = echo_upgrade_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, upstream_shell(address.port()))
        .await
        .expect("upstream");
    harness
        .control_plane()
        .create_route(&ctx, http_route(created.id, "/ws", &["GET"]))
        .await
        .expect("route");
    let socket_address = harness.serve_with(&ctx).await;

    let request = "GET /oagw/v1/ws/local/ws HTTP/1.1\r\nHost: gateway.example\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n".to_string();
    let head = send_over_socket(socket_address, &request, "gateway payload").await;
    assert!(head.contains("101"), "{head}");
    assert!(head.contains("upgrade"), "{head}");
}

#[tokio::test]
async fn the_ws_path_is_reachable_under_the_api_prefix_too() {
    let address = echo_upgrade_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, upstream_shell(address.port()))
        .await
        .expect("upstream");
    harness
        .control_plane()
        .create_route(&ctx, http_route(created.id, "/ws", &["GET"]))
        .await
        .expect("route");
    let socket_address = harness.serve_with(&ctx).await;

    let request = "GET /api/oagw/v1/ws/local/ws HTTP/1.1\r\nHost: gateway.example\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n".to_string();
    let head = send_over_socket(socket_address, &request, "second payload").await;
    assert!(head.contains("101"), "{head}");
}

#[tokio::test]
async fn an_upgrade_without_a_matching_route_is_a_404() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let socket_address = harness.serve_with(&ctx).await;

    let request = "GET /oagw/v1/ws/no-such-upstream/ws HTTP/1.1\r\nHost: gateway.example\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
    let head = send_over_socket(socket_address, request, "").await;
    assert!(head.contains("404"), "{head}");
}

#[tokio::test]
async fn a_non_upgrade_request_on_the_ws_path_is_proxied() {
    // `/ws/{alias}/{path}` handles plain requests through the same relay, so
    // the echo upstream answers them like any other proxy request.
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("upstream");

    let request = gateway_request("GET", "/oagw/v1/ws/local/v1/chat");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert!(body.starts_with("GET /v1/chat? "), "{body}");
}

/// Sends `head` to the gateway, then `payload` on the same connection, and
/// returns the response head plus whatever the upstream echoed back.
async fn send_over_socket(address: std::net::SocketAddr, request: &str, payload: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    socket.write_all(request.as_bytes()).await.expect("write");

    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = socket.read(&mut byte).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let deadline = std::time::Duration::from_millis(500);
    if !head.contains("101") {
        let mut rest = Vec::new();
        let wait = std::time::Duration::from_secs(1);
        if let Ok(Ok(count)) = tokio::time::timeout(wait, socket.read(&mut rest)).await {
            rest.truncate(count);
        }
        return format!(
            "{head}
--- body ---
{}",
            String::from_utf8_lossy(&rest)
        );
    }

    socket
        .write_all(payload.as_bytes())
        .await
        .expect("payload write");
    let mut echoed = Vec::new();
    let read = tokio::time::timeout(deadline, socket.read(&mut echoed)).await;
    if let Ok(Ok(count)) = read {
        echoed.truncate(count);
    }
    format!("{head}\n{}", String::from_utf8_lossy(&echoed))
}
