//! The plugin chain runs where the gateway owns the exchange.
//!
//! A bound guard is the last word before the upstream, a bound transform is the
//! first hand on the message, and a rejection is the chain's own answer rather
//! than the upstream's. These tests read the order the contract fixes —
//! auth, then guards, then the request transform, then the upstream, then the
//! response transform — out of what each stage leaves behind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use http_body_util::BodyExt;
use serde_json::json;

/// A route on `/v1/echo` bound to `plugins`.
async fn wired(
    app: &common::TestApp,
    upstream: &LocalUpstream,
    alias: &str,
    plugins: serde_json::Value,
) {
    wired_plugins(app, upstream, alias, plugins, json!([])).await;
}

/// A route on `/v1/echo` bound to `plugins`, forwarding `allowlist`.
async fn wired_plugins(
    app: &common::TestApp,
    upstream: &LocalUpstream,
    alias: &str,
    plugins: serde_json::Value,
    allowlist: serde_json::Value,
) {
    let spec = upstream.upstream_spec(alias);
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit"
        }))
        .await;
    let target = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/echo",
        "methods": ["GET", "POST"],
        "target_alias": target,
        "strip_prefix": false,
        "passthrough": "allowlist",
        "passthrough_allowlist": allowlist,
        "plugins": plugins
    }))
    .await;
}

/// Bind `required_headers` demanding `header`.
fn guard(headers: &[&str]) -> serde_json::Value {
    json!([{
        "plugin_id": "required_headers",
        "config": {"headers": headers}
    }])
}

/// Bind the `request_id` transform.
fn request_id() -> serde_json::Value {
    json!([{"plugin_id": "request_id", "config": {}}])
}

async fn proxy(
    app: &common::TestApp,
    alias: &str,
    headers: &[(&str, &str)],
) -> http::Response<axum::body::Body> {
    app.send(app.request(
        http::Method::GET,
        &format!("/oagw/v1/proxy/{alias}/v1/echo"),
        None,
        headers,
    ))
    .await
}

#[tokio::test]
async fn a_guard_refuses_a_request_lacking_the_header() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    wired(&app, &upstream, "guarded", guard(&["x-tenant-id"])).await;

    let response = proxy(&app, "guarded", &[]).await;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get(common::error_source_header())
            .and_then(|value| value.to_str().ok()),
        Some(common::error_source_gateway()),
        "the rejection is the chain's own, not the upstream's"
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem document");
    assert_eq!(document["status"], 400, "{document}");
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("x-tenant-id")),
        "the refusal names the header it wanted: {document}"
    );
    assert_eq!(upstream.count(), 0, "a refused request never travels");
}

/// The guard reads headers case-insensitively and lets a matching one through.
#[tokio::test]
async fn a_guard_allows_a_request_carrying_the_header() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    // The guard reads the caller's header; the route's allowlist is what puts
    // it on the forwarded request, which are two different decisions.
    let mut plugins = guard(&["X-Tenant-Id"]);
    plugins
        .as_array_mut()
        .expect("bindings")
        .push(json!({"plugin_id": "request_id", "config": {}}));
    wired_plugins(
        &app,
        &upstream,
        "guarded.ok",
        plugins,
        json!(["x-tenant-id"]),
    )
    .await;

    let response = proxy(&app, "guarded.ok", &[("x-tenant-id", "t-1")]).await;
    assert_eq!(response.status(), http::StatusCode::OK, "{response:?}");
    assert_eq!(upstream.count(), 1, "the request travelled");
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("x-tenant-id"), Some("t-1"));
}

/// A guard bound but configured with nothing fails open (ADR 0009).
#[tokio::test]
async fn a_guard_with_a_blank_configuration_fails_open() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    wired(
        &app,
        &upstream,
        "blank.guard",
        json!([{"plugin_id": "required_headers", "config": {}}]),
    )
    .await;

    let response = proxy(&app, "blank.guard", &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK, "{response:?}");
    assert_eq!(upstream.count(), 1);
}

#[tokio::test]
async fn the_caller_request_id_is_propagated_to_the_upstream() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    wired(&app, &upstream, "propagated", request_id()).await;

    let response = proxy(&app, "propagated", &[("x-request-id", "caller-abc-123")]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(
        received.header("x-request-id"),
        Some("caller-abc-123"),
        "the caller's identifier travels unchanged"
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("caller-abc-123"),
        "the same identifier is echoed for correlation"
    );
}

#[tokio::test]
async fn a_missing_request_id_is_minted_and_correlated() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    wired(&app, &upstream, "minted", request_id()).await;

    let response = proxy(&app, "minted", &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let forwarded = upstream.last().expect("the upstream was called");
    let minted = forwarded
        .header("x-request-id")
        .expect("an identifier was minted");
    assert_eq!(minted.len(), 32, "a minted identifier is 32 hex characters");
    assert!(
        minted.chars().all(|c| c.is_ascii_hexdigit()),
        "{minted} is hexadecimal"
    );
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some(minted),
        "the response carries the identifier the request was given"
    );
}

/// Two calls that bring no identifier get two different ones.
#[tokio::test]
async fn every_minted_identifier_is_fresh() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    wired(&app, &upstream, "fresh", request_id()).await;

    let first = proxy(&app, "fresh", &[]).await;
    let second = proxy(&app, "fresh", &[]).await;
    let first_id = first
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let second_id = second
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    assert_ne!(first_id, second_id, "identifiers are not reused");
}

/// Guards run before the response is built and the transform's error hook runs
/// after them: a refusal still leaves the exchange carrying an identifier.
#[tokio::test]
async fn the_chain_runs_auth_then_guards_then_transforms() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    wired(
        &app,
        &upstream,
        "ordered",
        json!([
            {"plugin_id": "request_id", "config": {}},
            {"plugin_id": "required_headers", "config": {"headers": ["x-tenant-id"]}}
        ]),
    )
    .await;

    // Missing the required header: the guard rejects, the upstream never sees
    // the exchange, and the error still carries a correlation identifier.
    let refused = proxy(&app, "ordered", &[]).await;
    assert_eq!(refused.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        refused
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::len),
        Some(32),
        "the transform's error hook ran after the guard rejected"
    );
    assert_eq!(upstream.count(), 0, "the guard short-circuits");

    // Supplying the header lets the same chain through, and the identifier the
    // transform minted on the request is what the upstream received.
    let allowed = proxy(&app, "ordered", &[("x-tenant-id", "t-1")]).await;
    assert_eq!(allowed.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    let identifier = received.header("x-request-id").expect("an identifier");
    assert_eq!(identifier.len(), 32);
    assert_eq!(
        allowed
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some(identifier),
        "the response echoes the forwarded identifier"
    );
}

/// A plugin the route disables is simply absent from the chain.
#[tokio::test]
async fn a_disabled_binding_does_not_run() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    wired(
        &app,
        &upstream,
        "disabled",
        json!([{
            "plugin_id": "required_headers",
            "config": {"headers": ["x-tenant-id"]},
            "enabled": false
        }]),
    )
    .await;

    let response = proxy(&app, "disabled", &[]).await;
    assert_eq!(
        response.status(),
        http::StatusCode::OK,
        "a disabled binding does not guard"
    );
    assert_eq!(upstream.count(), 1);
}

/// A binding whose id has no implementation is refused at configuration time,
/// not at the first exchange that would have stranded on it.
#[tokio::test]
async fn a_catalog_only_binding_is_refused_at_create_time() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("unimplemented");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit"
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/echo",
            "methods": ["GET"],
            "target_alias": alias,
            "strip_prefix": false,
            "plugins": [{"plugin_id": "basic", "config": {}}]
        }))
        .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{document}");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("plugin.not_found.v1")),
        "{document}"
    );
    assert_eq!(upstream.count(), 0, "nothing was ever forwarded");

    // The same refusal for an identifier the catalog does not know at all.
    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/echo",
            "methods": ["GET"],
            "target_alias": alias,
            "strip_prefix": false,
            "plugins": [{"plugin_id": "not_a_plugin", "config": {}}]
        }))
        .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{document}");
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("not_a_plugin")),
        "the refusal names the plugin: {document}"
    );
}
