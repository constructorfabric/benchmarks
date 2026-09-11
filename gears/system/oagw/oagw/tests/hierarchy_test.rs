//! The tenant-hierarchy walk (T063, T064).
//!
//! A descendant shadows its ancestors' aliases, an ancestor's upstream is
//! reachable through the proxy yet invisible through the management API, an
//! enforced ancestor limit cannot be relaxed, and an ancestor-disabled
//! upstream cannot be re-enabled by the descendant that shadows it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use httpmock::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

/// The descendant the requests come from (the harness tenant).
const CHILD: u128 = 1000;
/// The parent tenant.
const PARENT: u128 = 2000;
/// The root tenant.
const ROOT: u128 = 3000;

/// A hierarchical gear whose proxy dials `stub` on the child tenant.
async fn gear(stub: &MockServer) -> (Harness, String) {
    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);
    let upstream_id = create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    (harness, upstream_id)
}

async fn proxy(harness: &Harness, url: &str) -> axum::http::Response<axum::body::Body> {
    harness
        .send(harness.proxy_request("GET", url, &[], None))
        .await
}

/// A mock on `GET /v1/models` answering `ok`.
fn models(stub: &MockServer) -> httpmock::Mock<'_> {
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    })
}

// ---------------------------------------------------------------------------
// T063: the walk
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_descendant_shadows_an_ancestors_alias() {
    let ancestor = MockServer::start();
    let ancestor_models = models(&ancestor);
    let descendant = MockServer::start();
    let descendant_models = models(&descendant);

    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);

    // The parent registers the alias first, pointing at the ancestor stub.
    let parent_request = harness.request_for(
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("vendor.com", "127.0.0.1", ancestor.port(), "http")),
        Uuid::from_u128(PARENT),
    );
    let response = harness.send(parent_request).await;
    assert_eq!(response.status(), StatusCode::CREATED, "the parent registers its own upstream");

    // The child shadows the very same alias, pointing at the descendant stub.
    let upstream_id = create_upstream(&harness, "vendor.com", "127.0.0.1", descendant.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(response.status(), StatusCode::OK, "the child is served");
    assert_eq!(
        descendant_models.calls(),
        1,
        "the descendant's upstream was dialled"
    );
    assert_eq!(ancestor_models.calls(), 0, "the ancestor was shadowed, not dialled");
}

#[tokio::test]
async fn an_ancestor_without_a_shadowing_descendant_is_still_resolvable() {
    let ancestor = MockServer::start();
    let ancestor_models = models(&ancestor);

    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);

    // The parent registers the alias and a route; the child owns nothing.
    let created = harness
        .send(harness.request_for(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body("vendor.com", "127.0.0.1", ancestor.port(), "http")),
            Uuid::from_u128(PARENT),
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let parent_upstream = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    let parent_route = harness
        .send(harness.request_for(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(&parent_upstream, "/v1/models", &["GET"])),
            Uuid::from_u128(PARENT),
        ))
        .await;
    assert_eq!(parent_route.status(), StatusCode::CREATED, "the parent owns a route");

    let response = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(response.status(), StatusCode::OK, "the chain reaches the ancestor: {response:?}");
    assert_eq!(ancestor_models.calls(), 1, "the ancestor's upstream was dialled");
}

#[tokio::test]
async fn an_ancestors_upstream_is_invisible_through_the_management_api() {
    let ancestor = MockServer::start();
    let ancestor_models = models(&ancestor);

    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);
    let created = harness
        .send(harness.request_for(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body("vendor.com", "127.0.0.1", ancestor.port(), "http")),
            Uuid::from_u128(PARENT),
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let parent_upstream = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    create_route_for(&harness, Uuid::from_u128(PARENT), &parent_upstream, "/v1/models", &["GET"]).await;

    // The proxy walks the chain and dials the ancestor.
    let response = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(ancestor_models.calls(), 1);

    // The management API does not: a foreign id is a 404, never a listing.
    assert_eq!(
        harness
            .send(harness.request("GET", &format!("/oagw/v1/upstreams/{parent_upstream}"), None))
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "an ancestor's upstream is not readable by id"
    );
    let listed = read_json(harness.send(harness.request("GET", "/oagw/v1/upstreams", None)).await).await;
    let values = listed["value"].as_array().cloned().unwrap_or_default();
    assert!(
        !values.iter().any(|u| u["id"] == json!(parent_upstream)),
        "an ancestor's upstream is not listed: {values:?}"
    );
}

#[tokio::test]
async fn an_enforced_ancestor_limit_constrains_a_descendant() {
    let stub = MockServer::start();
    models(&stub);

    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);

    // The parent enforces a two-request-per-second ceiling on the alias.
    let parent_body = {
        let mut body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
        body["rate_limit"] = json!({
            "sharing": "enforce",
            "algorithm": "token_bucket",
            "sustained": { "rate": 2, "window": "second" },
            "scope": "global",
            "strategy": "reject"
        });
        body
    };
    let created = harness
        .send(harness.request_for(
            "POST",
            "/oagw/v1/upstreams",
            Some(parent_body),
            Uuid::from_u128(PARENT),
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    // The child shadows the alias with a much looser limit; the ancestor's
    // ceiling survives the shadowing.
    let upstream_id = create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let mut last = StatusCode::OK;
    let mut accepted = 0;
    for _ in 0..6 {
        let response = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
        last = response.status();
        if last == StatusCode::OK {
            accepted += 1;
        } else {
            break;
        }
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS, "the enforced ceiling still applies");
    assert_eq!(accepted, 2, "min(ancestor enforced 2, descendant 50) = 2, got {accepted}");
}

#[tokio::test]
async fn a_descendant_cannot_re_enable_an_ancestors_disabled_upstream() {
    let stub = MockServer::start();
    models(&stub);

    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);

    // The parent disables its upstream.
    let mut parent_body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
    parent_body["enabled"] = json!(false);
    let created = harness
        .send(harness.request_for("POST", "/oagw/v1/upstreams", Some(parent_body), Uuid::from_u128(PARENT)))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    // The child shadows it with a healthy upstream of its own.
    let upstream_id = create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;

    let response = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the ancestor's disabled state crosses the shadowing"
    );
    let problem: Value = read_json(response).await;
    assert_eq!(problem["type"], json!("gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"));
}

#[tokio::test]
async fn the_effective_configuration_carries_the_ancestor_union() {
    let stub = MockServer::start();
    models(&stub);

    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);

    let mut parent_body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
    parent_body["tags"] = json!(["platform"]);
    let created = harness
        .send(harness.request_for("POST", "/oagw/v1/upstreams", Some(parent_body), Uuid::from_u128(PARENT)))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let mut child_body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
    child_body["tags"] = json!(["team"]);
    let created = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(child_body)))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED, "shadowing an ancestor's alias is allowed");
    let child_upstream = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    // Tags are an add-only union: the child's own record keeps its tags, the
    // ancestor's survive in the ancestor's record, and neither can remove the
    // other's.
    let child_record = read_json(
        harness
            .send(harness.request("GET", &format!("/oagw/v1/upstreams/{child_upstream}"), None))
            .await,
    )
    .await;
    assert_eq!(child_record["tags"], json!(["team"]), "the child's tags are its own");

    // The hierarchy walk resolves the child's upstream, then folds the
    // ancestor's enforced layers on top.
    let chain = harness.state.tenant_chain(&harness.context()).await.expect("a chain");
    let resolved = harness
        .state
        .control_plane
        .resolve_alias(&chain, "vendor.com")
        .await
        .expect("resolution works")
        .expect("the alias resolves");
    assert_eq!(
        resolved.upstream.id.as_deref(),
        Some(child_upstream.as_str()),
        "the closest match wins"
    );
    let effective = harness
        .state
        .control_plane
        .effective_config(&chain, &resolved, None)
        .await
        .expect("an effective configuration");
    assert!(effective.tags.contains(&"team".to_string()), "{:?}", effective.tags);
    assert!(effective.tags.contains(&"platform".to_string()), "{:?}", effective.tags);
    assert_eq!(effective.alias, "vendor.com");
}

// ---------------------------------------------------------------------------
// T064: alias binding across the hierarchy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_alias_collides_only_within_a_tenant() {
    let stub = MockServer::start();

    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);

    let body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body.clone())))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    // The same tenant may not bind the alias twice.
    let response = harness
        .send(harness.request("POST", "/oagw/v1/upstreams", Some(body)))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT, "an alias is unique per tenant");
    let problem: Value = read_json(response).await;
    assert_eq!(problem["type"], json!("gts.cf.core.errors.err.v1~cf.oagw.conflict.v1"));

    // A different tenant of the same hierarchy may: it shadows.
    let response = harness
        .send(harness.request_for(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body("vendor.com", "127.0.0.1", stub.port(), "http")),
            Uuid::from_u128(PARENT),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED, "a parent may bind the same alias");
}

#[tokio::test]
async fn a_shadows_alias_resolution_prefers_the_closest_tenant() {
    let ancestor = MockServer::start();
    let descendant = MockServer::start();
    let resolver = Arc::new(StubResolver::lineage(
        Uuid::from_u128(CHILD),
        Uuid::from_u128(PARENT),
        Uuid::from_u128(ROOT),
    ));
    let harness = Harness::hierarchical(Harness::plaintext_config(), resolver);

    for (tenant, stub) in [
        (Uuid::from_u128(ROOT), &ancestor),
        (Uuid::from_u128(PARENT), &descendant),
        (Uuid::from_u128(CHILD), &ancestor),
    ] {
        let body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
        let response = harness
            .send(harness.request_for("POST", "/oagw/v1/upstreams", Some(body), tenant))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let chain = harness.state.tenant_chain(&harness.context()).await.expect("a chain");
    let resolved = harness
        .state
        .control_plane
        .resolve_alias(&chain, "VENDOR.COM")
        .await
        .expect("resolution works")
        .expect("the alias resolves");
    assert_eq!(resolved.owner_tenant_id, Uuid::from_u128(CHILD), "the child's own upstream wins");
    assert_eq!(resolved.chain.entries(), &[Uuid::from_u128(CHILD), Uuid::from_u128(PARENT), Uuid::from_u128(ROOT)]);
}

/// Creates a route for another tenant through the same harness.
async fn create_route_for(
    harness: &Harness,
    tenant: Uuid,
    upstream_id: &str,
    path: &str,
    methods: &[&str],
) -> String {
    let response = harness
        .send(harness.request_for(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(upstream_id, path, methods)),
            tenant,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED, "route created");
    read_json(response).await["id"].as_str().unwrap_or_default().to_string()
}
