//! Integration tests of the counter scoping of entry 2.7 over the proxy
//! surface (`cpt-cf-oagw-flow-rate-limiting-counter-scope`,
//! `cpt-cf-oagw-dod-rate-limiting-counter-scopes`,
//! `cpt-cf-oagw-dod-rate-limiting-per-instance-state`).
//!
//! Two tenants address the same alias over one shared data plane, so a budget
//! spent by one of them is never observed by the other unless the configured
//! scope pools them on purpose.

use std::sync::Arc;

use oagw::domain::dto::{
    BurstCapacity, EndpointScheme, HttpMethod, RateAlgorithm, RateLimitConfig, RateScope,
    RateStrategy, RateWindow, SustainedRate,
};
use oagw::test_support::{
    management_surface, permissive_surface, route_for, seed_route, seed_upstream, stub_upstream,
    upstream_at, FakeHierarchyTenantResolver, FakePolicyAuthZ,
};
use uuid::Uuid;

const PROXY: &str = "/oagw/v1/proxy";

fn proxy_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// A `token_bucket` limit of `rate` per second with `capacity` burst at
/// `scope`.
fn limit(rate: u32, capacity: u32, scope: RateScope) -> RateLimitConfig {
    RateLimitConfig {
        sharing: oagw::SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window: RateWindow::Second },
        burst: Some(BurstCapacity { capacity }),
        budget: None,
        scope,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

/// The surface with one upstream and one route per tenant of `tenants`, every
/// one of them addressing the same alias over the same stub upstream, carrying
/// the same limit.
async fn seeded(
    limit: RateLimitConfig,
    tenants: &[Uuid],
) -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream) {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    for tenant in tenants {
        let mut upstream = upstream_at(*tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
        upstream.rate_limit = Some(limit.clone());
        let upstream_id = seed_upstream(&surface, upstream);
        let route = route_for(*tenant, upstream_id, "/v1", &[HttpMethod::Get]);
        seed_route(&surface, route);
    }
    (surface, stub)
}

/// Drain the budget of `tenant`, then report whether the next request was
/// admitted.
async fn drained(surface: &oagw::test_support::ManagementSurface, tenant: Uuid) -> http::StatusCode {
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    for _ in 0..5 {
        let exchange = surface.proxy_for(tenant, Uuid::new_v4(), "GET", path, &[], b"").await;
        if exchange.status != http::StatusCode::OK {
            return exchange.status;
        }
    }
    surface.proxy_for(tenant, Uuid::new_v4(), "GET", path, &[], b"").await.status
}

/// Two tenants addressing the same alias never share a counter: exhausting the
/// budget of one leaves the other untouched
/// (`cpt-cf-oagw-flow-rate-limiting-counter-scope`).
#[tokio::test]
async fn two_tenants_never_share_a_budget() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let (surface, stub) = seeded(limit(1, 1, RateScope::Tenant), &[first, second]).await;
    assert_eq!(drained(&surface, first).await, http::StatusCode::TOO_MANY_REQUESTS);
    // The second tenant has its own counter, so its first request is admitted.
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let other = surface.proxy_for(second, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(other.status, http::StatusCode::OK, "{:?}", other.text());
    assert_eq!(other.header("x-ratelimit-remaining"), Some("0"));
    assert_eq!(stub.received().len(), 2, "only the refusals stay away from the upstream");
}

/// A tenant-scoped counter is not shared with a *different* tenant even when
/// both are refused in the same instant.
#[tokio::test]
async fn two_tenants_refused_in_the_same_instant_are_refused_independently() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let (surface, _stub) = seeded(limit(1, 1, RateScope::Tenant), &[first, second]).await;
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let exhausted = surface.proxy_for(first, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(exhausted.status, http::StatusCode::OK);
    let refused = surface.proxy_for(first, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS);
    let untouched = surface.proxy_for(second, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(untouched.status, http::StatusCode::OK, "{:?}", untouched.text());
}

/// The `global` scope pools every caller of one upstream on one counter, so
/// the budget of the upstream is spent once no matter who calls
/// (`cpt-cf-oagw-dod-rate-limiting-counter-scopes`).
#[tokio::test]
async fn the_global_scope_pools_every_caller_of_one_upstream_on_one_counter() {
    let tenant = Uuid::new_v4();
    let (surface, _stub) = seeded(limit(2, 2, RateScope::Global), &[tenant]).await;
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let first_call = surface.proxy_for(tenant, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(first_call.status, http::StatusCode::OK);
    assert_eq!(first_call.header("x-ratelimit-limit"), Some("2"));
    assert_eq!(first_call.header("x-ratelimit-remaining"), Some("1"));
    let second_call = surface.proxy_for(tenant, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(second_call.status, http::StatusCode::OK, "the second caller spends the same counter");
    assert_eq!(second_call.header("x-ratelimit-remaining"), Some("0"));
    let third = surface.proxy_for(tenant, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(third.status, http::StatusCode::TOO_MANY_REQUESTS, "the pooled budget is spent");
}

/// The `user` scope keys the counter by the caller, so one principal cannot
/// consume the budget of another inside the same tenant.
#[tokio::test]
async fn two_principals_of_one_tenant_never_share_a_budget() {
    let tenant = Uuid::new_v4();
    let (surface, _stub) = seeded(limit(1, 1, RateScope::User), &[tenant]).await;
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    for caller in [first, second] {
        let admitted = surface.proxy_for(tenant, caller, "GET", path, &[], b"").await;
        assert_eq!(admitted.status, http::StatusCode::OK, "{caller}");
        assert_eq!(admitted.header("x-ratelimit-remaining"), Some("0"));
        let refused = surface.proxy_for(tenant, caller, "GET", path, &[], b"").await;
        assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS, "{caller}");
    }
}

/// An upstream-level counter and a route-level counter never contend, so a
/// route limit is a second budget over the same upstream
/// (`cpt-cf-oagw-algo-rate-limiting-counter-key`).
#[tokio::test]
async fn an_upstream_and_its_route_never_share_a_budget() {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let mut upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    upstream.rate_limit = Some(limit(1, 1, RateScope::Tenant));
    let upstream_id = seed_upstream(&surface, upstream);
    let mut route = route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]);
    route.rate_limit = Some(limit(1, 1, RateScope::Tenant));
    seed_route(&surface, route);
    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let admitted = surface.proxy_for(tenant, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(admitted.status, http::StatusCode::OK);
    assert_eq!(admitted.header("x-ratelimit-limit"), Some("1"));
    let refused = surface.proxy_for(tenant, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(refused.status, http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("x-ratelimit-remaining"), Some("0"));
}

/// An enforced ancestor limit survives alias shadowing: the descendant
/// upstream is the routing target, but the ancestor budget is the one enforced
/// (`cpt-cf-oagw-dod-rate-limiting-hierarchical-inheritance`).
#[tokio::test]
async fn an_enforced_ancestor_limit_survives_alias_shadowing() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let surface = management_surface(
        proxy_config(),
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&[leaf, root]),
    )
    .await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();

    // The ancestor owns the alias first, with an enforced limit of one per
    // second.
    let mut ancestor =
        upstream_at(root, "api.vendor.com", EndpointScheme::Http, &host, port);
    let mut enforced = limit(1, 1, RateScope::Tenant);
    enforced.sharing = oagw::SharingMode::Enforce;
    ancestor.rate_limit = Some(enforced);
    let ancestor_id = seed_upstream(&surface, ancestor);
    seed_route(&surface, route_for(root, ancestor_id, "/v1", &[HttpMethod::Get]));

    // The descendant shadows the alias with a much looser limit of its own.
    let mut descendant =
        upstream_at(leaf, "api.vendor.com", EndpointScheme::Http, &host, port);
    let mut loose = limit(100, 100, RateScope::Tenant);
    loose.sharing = oagw::SharingMode::Inherit;
    descendant.rate_limit = Some(loose);
    let shadow_id = seed_upstream(&surface, descendant);
    seed_route(&surface, route_for(leaf, shadow_id, "/v1", &[HttpMethod::Get]));

    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    let first = surface.proxy_for(leaf, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(first.status, http::StatusCode::OK, "{:?}", first.text());
    assert_eq!(first.header("x-ratelimit-limit"), Some("1"), "the enforced ancestor limit stands");
    let second = surface.proxy_for(leaf, Uuid::new_v4(), "GET", path, &[], b"").await;
    assert_eq!(second.status, http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.header("x-ratelimit-limit"), Some("1"));
    assert_eq!(stub.received().len(), 1, "the refused request reached no upstream");
}

/// A `private` ancestor limit contributes nothing to a descendant, which is
/// then limited only by its own configuration.
#[tokio::test]
async fn a_private_ancestor_limit_is_invisible_to_a_descendant() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let surface = management_surface(
        proxy_config(),
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&[leaf, root]),
    )
    .await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let mut ancestor =
        upstream_at(root, "api.vendor.com", EndpointScheme::Http, &host, port);
    ancestor.rate_limit = Some(limit(1, 1, RateScope::Tenant));
    let ancestor_id = seed_upstream(&surface, ancestor);
    seed_route(&surface, route_for(root, ancestor_id, "/v1", &[HttpMethod::Get]));

    let mut descendant =
        upstream_at(leaf, "api.vendor.com", EndpointScheme::Http, &host, port);
    descendant.rate_limit = Some(limit(3, 3, RateScope::Tenant));
    let shadow_id = seed_upstream(&surface, descendant);
    seed_route(&surface, route_for(leaf, shadow_id, "/v1", &[HttpMethod::Get]));

    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    for _ in 0..3 {
        let exchange = surface.proxy_for(leaf, Uuid::new_v4(), "GET", path, &[], b"").await;
        assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
    }
    assert_eq!(
        surface.proxy_for(leaf, Uuid::new_v4(), "GET", path, &[], b"").await.status,
        http::StatusCode::TOO_MANY_REQUESTS,
        "the descendant budget applies, not the ancestor one"
    );
}

/// A three-level hierarchy composes the effective limit as the minimum over
/// the chain: the strictest layer wins for both the sustained rate and the
/// burst capacity (`cpt-cf-oagw-dod-rate-limiting-hierarchical-inheritance`).
#[tokio::test]
async fn a_three_level_hierarchy_takes_the_strictest_layer() {
    let root = Uuid::new_v4();
    let mid = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let surface = management_surface(
        proxy_config(),
        Arc::new(FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&[leaf, mid, root]),
    )
    .await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();

    for (tenant, rate, capacity) in [(root, 100u32, 100u32), (mid, 2, 2), (leaf, 100, 100)] {
        let mut upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
        let mut limit = limit(rate, capacity, RateScope::Tenant);
        limit.sharing = oagw::SharingMode::Inherit;
        upstream.rate_limit = Some(limit);
        let upstream_id = seed_upstream(&surface, upstream);
        seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));
    }

    let path = &format!("{PROXY}/api.vendor.com/v1/x");
    for _ in 0..2 {
        let exchange = surface.proxy_for(leaf, Uuid::new_v4(), "GET", path, &[], b"").await;
        assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
        assert_eq!(exchange.header("x-ratelimit-limit"), Some("2"), "the middle layer wins");
    }
    assert_eq!(
        surface.proxy_for(leaf, Uuid::new_v4(), "GET", path, &[], b"").await.status,
        http::StatusCode::TOO_MANY_REQUESTS,
        "min(system, partner, tenant) is the middle layer"
    );
}
