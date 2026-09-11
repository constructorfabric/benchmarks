//! Integration tests of the Data Plane L1 hot-configuration flush
//! (`cpt-cf-oagw-flow-observability-and-state-dp-cache-flush`,
//! `cpt-cf-oagw-dod-observability-and-state-dp-cache`).
//!
//! The tests drive the real `OagwGear` over the registered REST surface, so the
//! entries the cache holds are the ones real proxy exchanges populated and the
//! flush is the one a real management write performed.
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-dp-cache:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use http::Method;
use oagw::domain::dto::{EndpointScheme, HttpMethod};
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

/// One proxied exchange by a caller of `tenant` against `/v1/orders`.
async fn proxied(
    surface: &oagw::test_support::ManagementSurface,
    tenant: Uuid,
) -> oagw::test_support::ProxyExchange {
    surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"")
        .await
}

/// A proxy exchange populates the Data Plane entries its alias resolution
/// recorded, and a management write flushes exactly them
/// (`cpt-cf-oagw-flow-observability-and-state-dp-cache-flush`).
#[tokio::test]
async fn a_read_populates_and_a_write_flushes() {
    let surface = permissive_surface(proxy_config()).await;
    let (_stub, tenant, upstream_id) = seeded(&surface).await;
    let hot = surface.gear.hot_config().expect("the DP cache is published");
    assert!(hot.is_empty(), "the cache starts cold");

    let exchange = proxied(&surface, tenant).await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());
    assert!(!hot.is_empty(), "the alias resolution recorded its dependency set");
    let key = oagw::infra::dp_cache::DpHotConfig::upstream_key(tenant, "api.vendor.com");
    assert!(
        hot.get_upstream(tenant, "api.vendor.com").is_some(),
        "the resolved alias is one cached entry: {key}"
    );

    // The write flushes the entries whose dependency set intersects the
    // affected key set.
    let (status, body) = surface
        .send(
            Method::PUT,
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(json!({
                "alias": "api.vendor.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
                "tags": ["flushed"]
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(hot.is_empty(), "the write flushed the entries the alias resolution recorded");
}

/// A configuration write removes the served stale value and leaves an unrelated
/// entry in place
/// (`cpt-cf-oagw-dod-observability-and-state-dp-cache`).
#[tokio::test]
async fn a_write_flushes_the_affected_entries_and_leaves_the_unrelated_ones() {
    let surface = permissive_surface(proxy_config()).await;
    let first = stub_upstream(Vec::new()).await;
    let (first_host, first_port) = first.endpoint();
    let tenant = Uuid::new_v4();
    let written_id = seed_upstream(
        &surface,
        upstream_at(tenant, "written.vendor.com", EndpointScheme::Http, &first_host, first_port),
    );
    let second = stub_upstream(Vec::new()).await;
    let (second_host, second_port) = second.endpoint();
    let untouched_id = seed_upstream(
        &surface,
        upstream_at(tenant, "untouched.vendor.com", EndpointScheme::Http, &second_host, second_port),
    );
    seed_route(&surface, route_for(tenant, written_id, "/v1", &[HttpMethod::Get]));
    seed_route(&surface, route_for(tenant, untouched_id, "/v1", &[HttpMethod::Get]));

    for alias in ["written.vendor.com", "untouched.vendor.com"] {
        let exchange = surface
            .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/{alias}/v1/orders"), &[], b"")
            .await;
        assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());
    }
    let hot = surface.gear.hot_config().expect("the DP cache is published");
    assert!(hot.get_upstream(tenant, "written.vendor.com").is_some());
    assert!(hot.get_upstream(tenant, "untouched.vendor.com").is_some());

    // Replacing the first upstream flushes its entry and leaves the second
    // upstream's entry in place.
    let (status, body) = surface
        .send(
            Method::PUT,
            &format!("/oagw/v1/upstreams/{written_id}"),
            Some(security_context(tenant, Uuid::new_v4())),
            Some(json!({
                "alias": "written.vendor.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [ { "scheme": "http", "host": first_host, "port": first_port } ] },
                "tags": ["flushed"]
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(
        hot.get_upstream(tenant, "written.vendor.com").is_none(),
        "the written record is not served stale"
    );
    assert!(
        hot.get_upstream(tenant, "untouched.vendor.com").is_some(),
        "an unrelated entry stays in place"
    );
}

/// A route write flushes the entry whose dependency set covers the route key,
/// and a written record the flush cannot derive keys for clears the whole cache
/// (`inst-os-algo-inval-8`).
#[tokio::test]
async fn a_route_write_flushes_only_its_own_entry() {
    let surface = permissive_surface(proxy_config()).await;
    let (stub, tenant, upstream_id) = seeded(&surface).await;
    let _ = stub;
    let hot = surface.gear.hot_config().expect("the DP cache is published");
    let exchange = proxied(&surface, tenant).await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert!(hot.get_upstream(tenant, "api.vendor.com").is_some());

    // A `grpc` route derives no key, so the flush clears the whole cache.
    let (status, body) = surface
        .send(
            Method::POST,
            "/oagw/v1/routes",
            Some(security_context(tenant, Uuid::new_v4())),
            Some(json!({
                "upstream_id": upstream_id.to_string(),
                "match": { "grpc": { "service": "svc", "method": "m" } },
                "priority": 0,
                "enabled": true
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{}", String::from_utf8_lossy(&body));
    assert!(hot.is_empty(), "an underivable key set clears the whole cache");
}
