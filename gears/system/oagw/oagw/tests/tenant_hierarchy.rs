//! AT-10: the tenant hierarchy (`DESIGN.md` § "Hierarchical Configuration",
//! § "Tenant Scoping").
//!
//! Alias resolution walks the chain from the descendant to the root and the
//! closest match wins, but the ancestor constraints marked `enforce` survive
//! the shadowing, and the ancestor's own resources stay invisible through the
//! management API even while its alias stays reachable on the data plane.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use oagw::domain::services::management::HierarchyChain;

use common::net::{bind_upstream, read_request, write_response};
use common::{Caller, PROTOCOL_HTTP, harness_with_chain, send, upstream_body};

const PREFIX: &str = "gts.cf.core.errors.err.v1~";

/// A route on `path` admitting `GET`.
fn route(upstream_id: &str, path: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET"], "path": path,
            "query_allowlist": [], "path_suffix_mode": "append"
        }}
    })
    .to_string()
}

/// An upstream body whose rate limit carries a sharing mode.
fn limited_upstream(alias: &str, port: u16, rate: u32, window: &str, sharing: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "http", "host": "127.0.0.1", "port": port }
        ]},
        "protocol": PROTOCOL_HTTP,
        "rate_limit": {
            "sharing": sharing,
            "algorithm": "token_bucket",
            "sustained": { "rate": rate, "window": window },
            "burst": { "capacity": rate },
            "scope": "user",
            "strategy": "reject",
            "cost": 1
        }
    })
    .to_string()
}

/// A parent and a child tenant, the child a descendant of the parent.
fn chain() -> (Caller, Caller, axum::Router) {
    let parent = Caller::default();
    let child = Caller::default();
    let mut hierarchy = HierarchyChain::default();
    hierarchy.insert(child.tenant_id, vec![parent.tenant_id]);
    let harness = harness_with_chain(oagw::config::OagwConfig::default(), Arc::new(hierarchy));
    (parent, child, harness.app)
}

#[tokio::test]
async fn an_ancestors_resources_are_invisible_to_a_descendant_through_management() {
    let (parent, child, app) = chain();
    let addr = common::free_port().await;
    let alias = format!("hidden-{addr}");

    // The parent creates the upstream; the descendant can proxy it but never
    // address it through the management surface.
    let (status, _, created) = send(
        &app,
        &parent,
        "POST",
        "/oagw/v1/upstreams",
        Some(&upstream_body(&alias, addr)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_owned();
    let _ = send(
        &app,
        &parent,
        "POST",
        "/oagw/v1/routes",
        Some(&route(&id, "/v1")),
    )
    .await;

    // The ancestor's route is inherited at proxy time but not visible here.
    let (status, _, body) = send(
        &app,
        &child,
        "GET",
        &format!("/oagw/v1/routes?upstream_id={id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(0), "{body}");

    let (status, _, body) = send(
        &app,
        &child,
        "GET",
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, _, body) = send(
        &app,
        &child,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(&serde_json::json!({"enabled": false, "alias": alias}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, _, body) = send(
        &app,
        &child,
        "DELETE",
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // The listing is the caller's own, never the ancestor's.
    let (status, _, body) = send(&app, &child, "GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(0), "{body}");

    // On the data plane the alias is still reachable: the chain walk finds it.
    let (status, _, body) = send(
        &app,
        &child,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn a_descendant_shadows_the_ancestor_alias_and_wins_the_dial() {
    let (parent, child, app) = chain();
    let ancestor = bind_upstream().await;
    let own = bind_upstream().await;
    let alias = format!("shared-{}", own.0.port());

    let (status, _, created) = send(
        &app,
        &parent,
        "POST",
        "/oagw/v1/upstreams",
        Some(&upstream_body(&alias, ancestor.0.port())),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let parent_id = created["id"].as_str().expect("id").to_owned();
    let _ = send(
        &app,
        &parent,
        "POST",
        "/oagw/v1/routes",
        Some(&route(&parent_id, "/v1")),
    )
    .await;

    let (status, _, created) = send(
        &app,
        &child,
        "POST",
        "/oagw/v1/upstreams",
        Some(&upstream_body(&alias, own.0.port())),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let own_id = created["id"].as_str().expect("id").to_owned();
    let _ = send(
        &app,
        &child,
        "POST",
        "/oagw/v1/routes",
        Some(&route(&own_id, "/v1")),
    )
    .await;

    // Only the child's endpoint answers: the dial went to the closest match.
    let server = tokio::spawn(async move {
        let (mut stream, _) = own.1.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        write_response(
            &mut stream,
            "HTTP/1.1 200 OK",
            &[("x-owner", "child")],
            b"{}",
        )
        .await;
    });

    let (status, _, body) = send(
        &app,
        &child,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/chat"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    server.await.expect("the child's endpoint answered");
}

#[tokio::test]
async fn an_enforced_ancestor_rate_limit_survives_the_shadowing() {
    let (parent, child, app) = chain();
    let addr = common::free_port().await;
    let alias = "gated.vendor.test";

    // The ancestor declares a strict limit and enforces it.
    let (status, _, created) = send(
        &app,
        &parent,
        "POST",
        "/oagw/v1/upstreams",
        Some(&limited_upstream(alias, addr, 1, "minute", "enforce")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let parent_id = created["id"].as_str().expect("id").to_owned();
    let _ = send(
        &app,
        &parent,
        "POST",
        "/oagw/v1/routes",
        Some(&route(&parent_id, "/v1")),
    )
    .await;

    // The descendant shadows the alias with a limit of its own, an order of
    // magnitude looser, and its own route on the same path.
    let own_addr = common::free_port().await;
    let (status, _, created) = send(
        &app,
        &child,
        "POST",
        "/oagw/v1/upstreams",
        Some(&limited_upstream(alias, own_addr, 100, "minute", "private")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let own_id = created["id"].as_str().expect("id").to_owned();
    let _ = send(
        &app,
        &child,
        "POST",
        "/oagw/v1/routes",
        Some(&route(&own_id, "/v1")),
    )
    .await;

    // The ancestor's budget is the one that applies: the first request passes,
    // the second is rejected on the ancestor's rate.
    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    let (status, _, _) = send(&app, &child, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _, body) = send(&app, &child, "GET", &path, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        body["type"],
        format!("{PREFIX}cf.oagw.rate_limit.exceeded.v1")
    );
    assert_eq!(
        body["retry_after_seconds"], 60,
        "the enforced ancestor's window, not the descendant's"
    );
}

#[tokio::test]
async fn an_inherited_ancestor_rate_limit_fills_a_gap() {
    let (parent, child, app) = chain();
    let addr = common::free_port().await;
    let alias = "offered.vendor.test";

    let (status, _, created) = send(
        &app,
        &parent,
        "POST",
        "/oagw/v1/upstreams",
        Some(&limited_upstream(alias, addr, 1, "minute", "inherit")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let parent_id = created["id"].as_str().expect("id").to_owned();
    let _ = send(
        &app,
        &parent,
        "POST",
        "/oagw/v1/routes",
        Some(&route(&parent_id, "/v1")),
    )
    .await;

    // The descendant declares no rate limit of its own, so the ancestor's
    // inherited one applies.
    let own_addr = common::free_port().await;
    let (status, _, created) = send(
        &app,
        &child,
        "POST",
        "/oagw/v1/upstreams",
        Some(&upstream_body(alias, own_addr)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let own_id = created["id"].as_str().expect("id").to_owned();
    let _ = send(
        &app,
        &child,
        "POST",
        "/oagw/v1/routes",
        Some(&route(&own_id, "/v1")),
    )
    .await;

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    let (status, _, _) = send(&app, &child, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _, body) = send(&app, &child, "GET", &path, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["retry_after_seconds"], 60);
}
