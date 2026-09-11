//! Integration tests of the write-then-read ordering over the Control Plane L1
//! cache
//! (`cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`,
//! `cpt-cf-oagw-dod-observability-and-state-cp-cache`).
//!
//! The tests drive the real `OagwGear`, so the entry the cache holds is the one
//! a real proxy exchange populated and the invalidation is the one a real
//! management write performed, through the post-write hook the gear installs.
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-cp-cache:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use http::Method;
use oagw::domain::dto::{EndpointScheme, HttpMethod};
use oagw::infra::cp_cache::{CacheKey, CP_L1_CAPACITY};
use oagw::test_support::{
    permissive_surface, route_for, seed_route, seed_upstream, security_context, stub_upstream,
    upstream_at,
};
use serde_json::json;
use uuid::Uuid;

const PROXY: &str = "/oagw/v1/proxy";
const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// The `oagw` block the proxy tests need: `http` upstreams admitted.
fn proxy_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// The surface with one upstream and one route matching `/v1` over a stub
/// upstream.
async fn seeded(
    surface: &oagw::test_support::ManagementSurface,
) -> (oagw::test_support::StubUpstream, Uuid, Uuid) {
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    let upstream_id = seed_upstream(surface, upstream);
    seed_route(surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));
    (stub, tenant, upstream_id)
}

/// The Control Plane key of one alias owned by `tenant`.
fn key_of(tenant: Uuid) -> String {
    CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() }.as_string()
}

/// The single-executable mode constructs the Control Plane L1 cache with no
/// shared layer at all
/// (`cpt-cf-oagw-dod-observability-and-state-deployment-modes`).
#[tokio::test]
async fn the_cp_state_holds_l1_only() {
    let surface = permissive_surface(proxy_config()).await;
    let state = surface.gear.cp_state().expect("the CP state is published");
    assert!(state.l2_cache.is_none(), "no L2 layer exists in the graded configuration");
    assert_eq!(CP_L1_CAPACITY, 10_000, "the fixed capacity of the graded configuration");
    assert!(oagw::infra::cp_cache::refuse_l2().is_err(), "an L2 request is refused");
}

/// A proxy exchange populates the Control Plane L1 entry of the alias it
/// resolved, and a management write invalidates that entry before the operation
/// returns
/// (`cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`).
#[tokio::test]
async fn a_read_populates_and_a_write_invalidates() {
    let surface = permissive_surface(proxy_config()).await;
    let (stub, tenant, _upstream_id) = seeded(&surface).await;
    let (stub_host, stub_port) = stub.endpoint();
    let state = surface.gear.cp_state().expect("the CP state");
    let key = key_of(tenant);
    assert!(!state.l1.contains(&key), "the cache starts cold");

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert!(state.l1.contains(&key), "the alias resolution populated the entry");

    // The management write goes through the REST surface, so the post-write
    // hook runs after the store write and before the operation returns.
    let storage = surface.gear.storage().expect("the store");
    let (upstreams, _, _) = storage.repositories();
    let record = upstreams.get_by_alias(tenant, "api.vendor.com").expect("the upstream");
    let (status, _) = surface
        .send(
            Method::PUT,
            &format!("/oagw/v1/upstreams/{}", record.upstream.id),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(json!({
                "alias": "api.vendor.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [ { "scheme": "http", "host": stub_host, "port": stub_port } ] },
                "tags": ["billing"]
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{}", status);
    assert!(!state.l1.contains(&key), "the write invalidated the entry");

    // The read after the write repopulates the entry from the store.
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert!(state.l1.contains(&key), "the read after the write repopulated the entry");
}

/// The read after a write serves the written record, not the cached one, so a
/// proxied request reaches the endpoint the replacement named
/// (`cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`).
#[tokio::test]
async fn the_read_after_a_write_serves_the_written_record() {
    let surface = permissive_surface(proxy_config()).await;
    let first = stub_upstream(Vec::new()).await;
    let (host, port) = first.endpoint();
    let tenant = Uuid::new_v4();
    let upstream_id =
        seed_upstream(&surface, upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port));
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(first.received().len(), 1, "the cached record reached the first endpoint");

    // A second stub, and a management replacement pointing the alias at it: the
    // entry the cache held was invalidated, so the next exchange reaches the
    // second upstream.
    let second = stub_upstream(Vec::new()).await;
    let (second_host, second_port) = second.endpoint();
    let (status, body) = surface
        .send(
            Method::PUT,
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(json!({
                "alias": "api.vendor.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [ { "scheme": "http", "host": second_host, "port": second_port } ] }
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());
    assert_eq!(second.received().len(), 1, "the written record is the one served");
    assert_eq!(first.received().len(), 1, "the stale record was not served again");
}
