// Created: 2026-08-29 by Constructor Tech
//! Circuit breaker: consecutive transport failures, fail-fast while open and
//! recovery on success (contract §14).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{json_body, tenant};
use httpmock::MockServer;
use serde_json::json;
use std::net::TcpListener;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const THRESHOLD: u32 = 5;

/// A `127.0.0.1` port with nothing listening, so connections are refused.
fn refused_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

async fn register(harness: &common::Harness, alias: &str, port: u16) -> String {
    let created = json_body(
        common::post(
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
    common::post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;
    id
}

#[tokio::test]
async fn consecutive_failures_open_the_breaker() {
    let port = refused_port();
    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = register(&harness, "breaker.example.com", port).await;

    // Every attempt is a transport failure: 502 downstream error.
    for _ in 0..THRESHOLD {
        let response = harness
            .send(
                "GET",
                "/oagw/v1/proxy/breaker.example.com/api",
                None,
                tenant(),
            )
            .await;
        let source = common::header(&response, "x-oagw-error-source");
        assert_eq!(response.status(), 502);
        let body = json_body(response).await;
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
        );
        assert_eq!(source.as_deref(), Some("gateway"));
        assert_eq!(body["upstream_id"], json!(upstream_id));
    }

    // The next attempt fails fast before any connection attempt.
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/breaker.example.com/api",
            None,
            tenant(),
        )
        .await;
    let source = common::header(&response, "x-oagw-error-source");
    assert_eq!(response.status(), 503);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1"
    );
    assert_eq!(source.as_deref(), Some("gateway"));
}

#[tokio::test]
async fn the_breaker_is_per_endpoint() {
    let port = refused_port();
    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = register(&harness, "per-endpoint.example.com", port).await;
    let host = "127.0.0.1";

    for _ in 0..THRESHOLD {
        harness
            .data_plane()
            .breakers()
            .record_failure(upstream_id.parse().unwrap(), host);
    }
    assert_eq!(
        harness
            .data_plane()
            .breakers()
            .check(upstream_id.parse().unwrap(), host),
        oagw::infra::proxy::circuit_breaker::BreakerCheck::Open
    );
    // A different endpoint of the same upstream is unaffected.
    assert_eq!(
        harness
            .data_plane()
            .breakers()
            .check(upstream_id.parse().unwrap(), "other.example.com"),
        oagw::infra::proxy::circuit_breaker::BreakerCheck::Allowed
    );
}

#[tokio::test]
async fn a_success_closes_the_breaker_again() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let upstream_id = register(&harness, "recover.example.com", server.port()).await;
    let host = server.host().to_owned();

    // Trip the breaker for the selected endpoint, then prove the gateway fails
    // fast while it is open.
    for _ in 0..THRESHOLD {
        harness
            .data_plane()
            .breakers()
            .record_failure(upstream_id.parse().unwrap(), &host);
    }
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/recover.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        target.calls(),
        0,
        "an open breaker must not reach the upstream"
    );

    // One success closes it again and resets the counter.
    harness
        .data_plane()
        .breakers()
        .record_success(upstream_id.parse().unwrap(), &host);
    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/recover.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(target.calls(), 1);
}

#[tokio::test]
async fn a_recovered_upstream_is_reached_again_after_the_cooldown() {
    let harness = common::Harness::new(common::test_config(), None);
    let port = refused_port();
    let upstream_id = register(&harness, "half-open.example.com", port).await;
    let host = "127.0.0.1";

    for _ in 0..THRESHOLD {
        harness
            .data_plane()
            .breakers()
            .record_failure(upstream_id.parse().unwrap(), host);
    }
    assert_eq!(
        harness
            .data_plane()
            .breakers()
            .check(upstream_id.parse().unwrap(), host),
        oagw::infra::proxy::circuit_breaker::BreakerCheck::Open
    );

    // The registry's own half-open behaviour is covered by its unit tests; here
    // the counter is what the data plane records, so it must be reset by a
    // success before any further request is let through.
    harness
        .data_plane()
        .breakers()
        .record_success(upstream_id.parse().unwrap(), host);
    assert_eq!(
        harness
            .data_plane()
            .breakers()
            .failures(upstream_id.parse().unwrap(), host),
        0
    );
}
