//! Enable/disable semantics (T047): a disabled upstream is a `503` link
//! problem rather than a dial, a disabled route drops out of matching, a
//! disabled ancestor disables its descendants, and the owner can switch it
//! back on.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::*;
use httpmock::prelude::*;
use serde_json::json;

/// A gear whose upstream dials `stub`, created with `enabled` as given.
async fn gear(stub: &MockServer, alias: &str, enabled: bool) -> (Harness, String) {
    let harness = Harness::plaintext_gear();
    let mut body = upstream_body(alias, "127.0.0.1", stub.port(), "http");
    body["enabled"] = json!(enabled);
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = read_json(response).await["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (harness, id)
}

/// The `GET /v1/models` mock a proxied request is expected to reach.
fn models_mock(stub: &MockServer) -> httpmock::Mock {
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    })
}

async fn proxy(harness: &Harness, alias: &str) -> axum::http::Response<axum::body::Body> {
    harness
        .send(harness.proxy_request(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/v1/models"),
            &[],
            None,
        ))
        .await
}

fn header(response: &axum::http::Response<axum::body::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

#[tokio::test]
async fn a_disabled_upstream_returns_503_without_being_dialled() {
    let stub = MockServer::start();
    let mock = models_mock(&stub);
    let (harness, upstream_id) = gear(&stub, "offline", false).await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = proxy(&harness, "offline").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(mock.calls(), 0, "a disabled upstream is never dialled");

    let problem = read_json(response).await;
    assert_eq!(problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
    assert_eq!(problem["status"], 503);
    assert_eq!(problem["title"], "Link Unavailable");
    assert_eq!(problem["upstream_id"], format!("gts.cf.core.oagw.upstream.v1~{upstream_id}"));
    assert_eq!(problem["alias"], "offline");
}

#[tokio::test]
async fn re_enabling_the_upstream_restores_the_proxy() {
    let stub = MockServer::start();
    let mock = models_mock(&stub);
    let (harness, upstream_id) = gear(&stub, "flappy", false).await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    assert_eq!(proxy(&harness, "flappy").await.status(), StatusCode::SERVICE_UNAVAILABLE);

    // The owning tenant turns it back on.
    let enabled = json!({
        "id": upstream_id,
        "enabled": true,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": stub.port() } ] },
        "protocol": "http",
        "alias": "flappy"
    });
    let response = harness
        .send(harness.request(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(enabled),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "re-enabled");
    assert_eq!(proxy(&harness, "flappy").await.status(), StatusCode::OK);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_disabled_route_is_excluded_from_matching() {
    let stub = MockServer::start();
    // Both prefixes are served: only the route table decides which is used.
    stub.mock(|when, then| {
        when.method(GET);
        then.status(200).body("ok");
    });
    let (harness, upstream_id) = gear(&stub, "gated", true).await;
    let open = create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    // A second, disabled route on the longer prefix.
    let mut body = route_body(&upstream_id, "/v1/models/gpt-4", &["GET"]);
    body["enabled"] = json!(false);
    let response = harness
        .send(harness.request("POST", "/oagw/v1/routes", Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED, "a disabled route is storable");

    // The disabled route is not a candidate: its longer prefix cannot win.
    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/gated/v1/models/gpt-4", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "the enabled route still matches");
    assert_eq!(
        response.headers().get("x-oagw-route-id").and_then(|v| v.to_str().ok()),
        None
    );

    // Disabling the only route removes it from matching entirely.
    let response = harness
        .send(harness.request("PUT", &format!("/oagw/v1/routes/{open}"), Some(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/models",
                                 "path_suffix_mode": "append" } },
            "enabled": false
        }))))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = harness
        .send(harness.proxy_request("GET", "/oagw/v1/proxy/gated/v1/models", &[], None))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "no route matches any more");
    let problem = read_json(response).await;
    assert_eq!(problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
}

#[tokio::test]
async fn a_disabled_upstream_is_still_readable_through_the_management_api() {
    // Disabled is a data-plane posture, not a management-API deletion: the
    // owner can still read and update it.
    let stub = MockServer::start();
    let (harness, upstream_id) = gear(&stub, "paused", false).await;
    let response = harness
        .send(harness.request("GET", &format!("/oagw/v1/upstreams/{upstream_id}"), None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = read_json(response).await;
    assert_eq!(body["enabled"], false);
}
