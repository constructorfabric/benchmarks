//! AT-9: enable/disable semantics (`contracts/proxy-api.md` § 1, `DESIGN.md`
//! § 3.6).
//!
//! A disabled upstream answers `503 link.unavailable`; a disabled route simply
//! stops matching, so an unmatched path answers `404`; re-enabling restores the
//! previous behaviour exactly.

mod common;

use axum::http::StatusCode;
use common::{Caller, app, create_route, create_upstream, route_body, send, upstream_body};

const PREFIX: &str = "gts.cf.core.errors.err.v1~";

/// The alias a freshly created upstream carries.
async fn bound_upstream(
    app: &axum::Router,
    caller: &Caller,
    alias: &str,
    port: u16,
) -> serde_json::Value {
    create_upstream(app, caller, &upstream_body(alias, port)).await
}

#[tokio::test]
async fn a_disabled_upstream_is_unavailable_not_missing() {
    let addr = common::free_port().await;
    let alias = format!("off-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = bound_upstream(&app, &caller, &alias, addr).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;
    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");

    // Enabled: the request reaches the dial and the closed port answers 503
    // through the same error shape the disabled case uses — the difference is
    // the body, which is absent here because the upstream never answered.
    let (status, _, _) = send(&app, &caller, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // Disable it: the alias still resolves, so the answer is still 503, but now
    // the gateway is the one refusing.
    let disabled = serde_json::json!({
        "enabled": false,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "http", "host": "127.0.0.1", "port": addr }
        ]},
        "protocol": common::PROTOCOL_HTTP,
    })
    .to_string();
    let (status, _, body) = send(
        &app,
        &caller,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(&disabled),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);

    let (status, headers, body) = send(&app, &caller, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.link.unavailable.v1"));
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );

    // Re-enabling restores the previous behaviour: the dial is attempted again.
    let enabled = serde_json::json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "http", "host": "127.0.0.1", "port": addr }
        ]},
        "protocol": common::PROTOCOL_HTTP,
    })
    .to_string();
    let (status, _, body) = send(
        &app,
        &caller,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(&enabled),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);

    let (status, _, _) = send(&app, &caller, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_disabled_route_stops_matching_and_reenabling_restores_it() {
    let addr = common::free_port().await;
    let alias = format!("route-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = bound_upstream(&app, &caller, &alias, addr).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let route = serde_json::json!({
        "enabled": false,
        "upstream_id": id,
        "match": { "http": {
            "methods": ["GET"], "path": "/v1",
            "query_allowlist": [], "path_suffix_mode": "append"
        }}
    })
    .to_string();
    let created = create_route(&app, &caller, &route).await;
    let route_id = created["id"].as_str().expect("route id").to_owned();

    // Disabled: the path matches nothing, so the answer is a route miss.
    let (status, _, body) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/chat"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.route.not_found.v1"));

    // Enable it and the same request now gets as far as the dial.
    let enabled = serde_json::json!({
        "enabled": true,
        "upstream_id": id,
        "match": { "http": {
            "methods": ["GET"], "path": "/v1",
            "query_allowlist": [], "path_suffix_mode": "append"
        }}
    })
    .to_string();
    let (status, _, body) = send(
        &app,
        &caller,
        "PUT",
        &format!("/oagw/v1/routes/{route_id}"),
        Some(&enabled),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, _) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/chat"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // And disabling it again returns to the miss.
    let disabled = serde_json::json!({
        "enabled": false,
        "upstream_id": id,
        "match": { "http": {
            "methods": ["GET"], "path": "/v1",
            "query_allowlist": [], "path_suffix_mode": "append"
        }}
    })
    .to_string();
    let (status, _, _) = send(
        &app,
        &caller,
        "PUT",
        &format!("/oagw/v1/routes/{route_id}"),
        Some(&disabled),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/chat"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_alias_is_a_miss_not_an_outage() {
    let app = app();
    let caller = Caller::default();

    let (status, _, body) = send(&app, &caller, "GET", "/oagw/v1/proxy/absent/v1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["type"], format!("{PREFIX}cf.oagw.route.not_found.v1"));
}

#[tokio::test]
async fn a_management_surface_of_a_disabled_upstream_keeps_its_routes() {
    let addr = common::free_port().await;
    let alias = format!("keep-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = bound_upstream(&app, &caller, &alias, addr).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    // Disabling is not deleting: the routes remain listed.
    let disabled = serde_json::json!({
        "enabled": false,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "http", "host": "127.0.0.1", "port": addr }
        ]},
        "protocol": common::PROTOCOL_HTTP,
    })
    .to_string();
    let (status, _, _) = send(
        &app,
        &caller,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(&disabled),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, routes) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/routes?upstream_id={id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{routes}");
    assert_eq!(routes.as_array().map(Vec::len), Some(1));
}
