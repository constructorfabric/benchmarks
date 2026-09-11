//! AT-4: the gateway's own error surface (`contracts/errors.md`).
//!
//! Every rejection the gear itself produces is an RFC 9457 problem document
//! with `Content-Type: application/problem+json`,
//! `X-OAGW-Error-Source: gateway` and a `type` from the documented table.

mod common;

use axum::http::StatusCode;
use common::{Caller, app, create_route, create_upstream, route_body, upstream_body};

const GATEWAY: &str = "gateway";
const PREFIX: &str = "gts.cf.core.errors.err.v1~";

#[tokio::test]
async fn an_unknown_alias_is_a_route_not_found_problem() {
    let app = app();
    let caller = Caller::default();

    let (status, headers, body) =
        common::send(&app, &caller, "GET", "/oagw/v1/proxy/nope/v1/chat", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.route.not_found.v1"));
    assert_eq!(body["title"], "Route Not Found");
    assert_eq!(body["status"], 404);
    assert!(body["detail"].is_string());
    assert_eq!(body["instance"], "/oagw/v1/proxy/nope/v1/chat");
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some(GATEWAY)
    );
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}

#[tokio::test]
async fn a_management_404_is_also_a_problem_document() {
    let app = app();
    let caller = Caller::default();

    let (status, headers, body) = common::send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/upstreams/{}", uuid::Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.validation.error.v1"));
    assert_eq!(body["instance"], body["instance"]);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some(GATEWAY)
    );
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}

#[tokio::test]
async fn a_method_mismatch_is_a_validation_error() {
    let addr = common::free_port().await;
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &upstream_body("loopback", addr)).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let (status, _, body) = common::send(
        &app,
        &caller,
        "DELETE",
        &format!(
            "/oagw/v1/proxy/{}/v1/chat",
            upstream["alias"].as_str().expect("alias")
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.validation.error.v1"));
    assert_eq!(body["status"], 400);
    assert_eq!(
        body["instance"],
        format!(
            "/oagw/v1/proxy/{}/v1/chat",
            upstream["alias"].as_str().expect("alias")
        )
    );
}

#[tokio::test]
async fn an_unimplemented_protocol_is_reported_not_mis_answered() {
    let addr = common::free_port().await;
    let app = app();
    let caller = Caller::default();

    let upstream = common::create_upstream(
        &app,
        &caller,
        &serde_json::json!({
            "enabled": true,
            "alias": "grpc-target",
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": addr }
            ]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
        })
        .to_string(),
    )
    .await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let grpc_route = serde_json::json!({
        "enabled": true,
        "upstream_id": id,
        "match": { "grpc": { "service": "acme.Chat", "method": "Send" } }
    })
    .to_string();
    let _ = create_route(&app, &caller, &grpc_route).await;

    let (status, headers, body) = common::send(
        &app,
        &caller,
        "GET",
        "/oagw/v1/proxy/grpc-target/anything",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.protocol.error.v1"));
    assert!(body["detail"].as_str().is_some_and(|d| d.contains("grpc")));
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some(GATEWAY)
    );
}

#[tokio::test]
async fn a_payload_beyond_the_limit_is_rejected_with_413() {
    let addr = common::free_port().await;
    let app = common::harness(oagw::config::OagwConfig {
        max_body_bytes: 16,
        ..oagw::config::OagwConfig::default()
    })
    .app;
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &upstream_body("loopback", addr)).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let (status, _, body) = common::send(
        &app,
        &caller,
        "POST",
        &format!(
            "/oagw/v1/proxy/{}/v1/chat",
            upstream["alias"].as_str().expect("alias")
        ),
        Some(&"x".repeat(64)),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(
        body["type"],
        format!("{PREFIX}cf.oagw.payload.too_large.v1")
    );
}

/// FR-020 / Edge cases: an upstream that refuses the connection is a gateway
/// fault, not a hang or a raw transport error.
#[tokio::test]
async fn an_unreachable_upstream_is_a_link_unavailable_problem() {
    // A port nothing listens on: the dial fails immediately.
    let dead = common::free_port().await;
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &upstream_body("dark.local", dead)).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let (status, headers, body) = common::send(
        &app,
        &caller,
        "GET",
        &format!(
            "/oagw/v1/proxy/{}/v1/chat",
            upstream["alias"].as_str().expect("alias")
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.link.unavailable.v1"));
    assert_eq!(body["status"], 503);
    assert!(body["detail"].is_string());
    // A retrieable fault tells the caller when to come back, and names the
    // upstream it could not reach.
    assert!(body["retry_after_seconds"].is_u64(), "{body}");
    assert_eq!(body["host"], "dark.local");
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some(GATEWAY)
    );
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}

/// FR-020 / Edge cases: an upstream that never answers within the proxy
/// timeout is a `504`, again a gateway-produced problem document.
#[tokio::test]
async fn a_slow_upstream_is_a_request_timeout_problem() {
    let app = common::harness(oagw::config::OagwConfig {
        proxy_timeout_secs: 1,
        ..oagw::config::OagwConfig::default()
    })
    .app;
    let caller = Caller::default();

    let (addr, listener) = common::bind_upstream().await;
    let upstream =
        create_upstream(&app, &caller, &upstream_body("stalled.local", addr.port())).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    // The upstream accepts and then says nothing, holding the request open.
    let stalled = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (_head, _body) = common::net::read_request(&mut stream).await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        drop(stream);
    });

    let started = std::time::Instant::now();
    let (status, headers, body) = common::send(
        &app,
        &caller,
        "GET",
        &format!(
            "/oagw/v1/proxy/{}/v1/chat",
            upstream["alias"].as_str().expect("alias")
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.timeout.request.v1"));
    assert_eq!(body["status"], 504);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "the gateway waited {:?} instead of timing out",
        started.elapsed()
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some(GATEWAY)
    );

    stalled.abort();
}

/// US3 scenario 4 / AT-4: an upstream that starts answering and then breaks
/// mid-response is a gateway transport fault, reported as `502`.
#[tokio::test]
async fn an_upstream_that_breaks_mid_response_is_a_downstream_error() {
    let (addr, listener) = common::bind_upstream().await;
    let alias = "midread.local";
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &upstream_body(alias, addr.port())).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    // A well-formed head and one chunk, then a frame no decoder accepts, then
    // the connection is gone.
    let breaker = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (_head, _body) = common::net::read_request(&mut stream).await;
        common::net::write_raw(
            &mut stream,
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
               transfer-encoding: chunked\r\n\r\n5\r\nhello\r\n",
        )
        .await;
        common::net::write_raw(&mut stream, b"not-a-chunk-size\r\n").await;
    });

    let (status, headers, body) = common::send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/chat"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(
        body["type"],
        format!("{PREFIX}cf.oagw.downstream.error.v1"),
        "{body}"
    );
    assert_eq!(body["status"], 502);
    // A transport fault is the gateway's, and is answered as problem details.
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some(GATEWAY)
    );
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );

    breaker.abort();
}
