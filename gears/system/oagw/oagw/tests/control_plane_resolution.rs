//! Integration tests for the control-plane aggregate's request-path signature
//! `resolve_proxy_target(alias, method, path)` — the ADR 0006 single-tenant
//! shape (`cpt-cf-oagw-flow-request-proxy-target-host-selection`).
//!
//! The data plane walks the tenant chain through [`oagw::DataPlaneService`];
//! this file pins the shared control-plane aggregate's own resolution so the
//! two cannot silently disagree about what a route admits.
//!
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-integration-tests:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use oagw::domain::dto::{BurstCapacity, RateAlgorithm, RateScope, RateStrategy, RateWindow, RouteMatchType, SharingMode, SustainedRate};
use oagw::domain::{DomainError, RateLimitConfig};
use oagw::test_support::{route, test_context, upstream};
use oagw::OagwGear;
use toolkit::Gear;

async fn service_of() -> (
    uuid::Uuid,
    std::sync::Arc<dyn oagw::ControlPlaneService>,
) {
    let gear = OagwGear::default();
    gear.init(&test_context(None)).await.expect("init succeeds");
    (uuid::Uuid::new_v4(), gear.service().expect("service published"))
}

fn token_bucket(rate: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window: RateWindow::Second },
        burst: Some(BurstCapacity { capacity: rate }),
        budget: None,
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

/// A proxy target resolves by alias to the effective upstream configuration
/// plus the matched route, and the suffix past the matched prefix stays with
/// the caller's own path argument.
#[tokio::test]
async fn a_target_resolves_by_alias_to_the_effective_upstream_and_the_matched_route() {
    let (tenant, service) = service_of().await;
    let created = service
        .create_upstream(tenant, upstream(tenant, "payments"))
        .expect("the upstream is created");
    let route = service
        .create_route(tenant, route(created.id, "/v1/orders"))
        .expect("the route is created");

    let target = service
        .resolve_proxy_target(tenant, "payments", "GET", "/v1/orders/42")
        .expect("the target resolves");

    assert_eq!(target.route_id, route.id);
    assert_eq!(target.match_type, RouteMatchType::Http);
    assert!(
        !target.upstream.server.expect("the server block").endpoints.is_empty(),
        "the effective configuration carries the endpoint pool"
    );
}

/// A method outside the matched route's allowlist is not a match, so the
/// resolution reports the route as absent rather than admitting the method.
#[tokio::test]
async fn a_method_outside_the_allowlist_is_not_a_match() {
    let (tenant, service) = service_of().await;
    let created = service
        .create_upstream(tenant, upstream(tenant, "payments"))
        .expect("the upstream is created");
    service
        .create_route(tenant, route(created.id, "/v1/orders"))
        .expect("the route is created");

    let error = service
        .resolve_proxy_target(tenant, "payments", "DELETE", "/v1/orders/42")
        .expect_err("DELETE is outside the GET allowlist");
    assert!(matches!(error, DomainError::RouteNotFound { .. }), "{error}");
}

/// A disabled upstream resolves to nothing: the alias is found but the target
/// is unavailable, and no route of it is consulted.
#[tokio::test]
async fn a_disabled_upstream_is_unavailable_and_not_matched() {
    let (tenant, service) = service_of().await;
    let mut record = upstream(tenant, "payments");
    record.enabled = false;
    let created = service
        .create_upstream(tenant, record)
        .expect("the upstream is created");
    service
        .create_route(tenant, route(created.id, "/v1/orders"))
        .expect("the route is created");

    let error = service
        .resolve_proxy_target(tenant, "payments", "GET", "/v1/orders/42")
        .expect_err("the upstream is disabled");
    assert!(matches!(error, DomainError::LinkUnavailable { .. }), "{error}");
}

/// An unknown alias, a foreign tenant's alias and a path no route covers all
/// resolve to nothing, and the foreign case is indistinguishable from the
/// missing one.
#[tokio::test]
async fn an_unknown_alias_a_foreign_tenant_and_an_unmatched_path_resolve_to_nothing() {
    let (tenant, service) = service_of().await;
    let created = service
        .create_upstream(tenant, upstream(tenant, "payments"))
        .expect("the upstream is created");
    service
        .create_route(tenant, route(created.id, "/v1/orders"))
        .expect("the route is created");
    let stranger = uuid::Uuid::new_v4();

    let unknown = service
        .resolve_proxy_target(tenant, "no-such-alias", "GET", "/v1/orders")
        .expect_err("the alias is unknown");
    assert!(matches!(unknown, DomainError::NotFound { .. }), "{unknown}");

    let foreign = service
        .resolve_proxy_target(stranger, "payments", "GET", "/v1/orders")
        .expect_err("the upstream belongs to another tenant");
    assert!(matches!(foreign, DomainError::NotFound { .. }), "{foreign}");

    let unmatched = service
        .resolve_proxy_target(tenant, "payments", "GET", "/v2/other")
        .expect_err("no route covers the path");
    assert!(matches!(unmatched, DomainError::RouteNotFound { .. }), "{unmatched}");
}

/// A disabled route drops out of the resolution, so the remaining route of the
/// upstream is the one matched.
#[tokio::test]
async fn a_disabled_route_drops_out_of_the_resolution() {
    let (tenant, service) = service_of().await;
    let created = service
        .create_upstream(tenant, upstream(tenant, "payments"))
        .expect("the upstream is created");
    let mut retired = route(created.id, "/v1/orders");
    retired.id = uuid::Uuid::new_v4();
    retired.tenant_id = tenant;
    retired.enabled = false;
    service.create_route(tenant, retired).expect("the disabled route is stored");
    let mut live = route(created.id, "/v1/refunds");
    live.id = uuid::Uuid::new_v4();
    live.tenant_id = tenant;
    let live = service
        .create_route(tenant, live)
        .expect("the live route is created");

    let target = service
        .resolve_proxy_target(tenant, "payments", "GET", "/v1/refunds")
        .expect("the live route resolves");
    assert_eq!(target.route_id, live.id);
}

/// The resolved upstream is the *effective* configuration: a route that owns
/// its tenant identity overlays its rate-limit block onto the upstream base
/// layer, so the proxy path sees the merged posture and not the base one.
#[tokio::test]
async fn the_resolved_upstream_carries_the_route_overlay() {
    let (tenant, service) = service_of().await;
    let created = service
        .create_upstream(tenant, upstream(tenant, "payments"))
        .expect("the upstream is created");
    let mut overlaid = route(created.id, "/v1/orders");
    overlaid.id = uuid::Uuid::new_v4();
    overlaid.tenant_id = tenant;
    overlaid.rate_limit = Some(token_bucket(10));
    service
        .create_route(tenant, overlaid)
        .expect("the overlaying route is created");

    let target = service
        .resolve_proxy_target(tenant, "payments", "GET", "/v1/orders")
        .expect("the target resolves");
    let effective = target.upstream.rate_limit.expect("the route layer contributes one");
    assert_eq!(effective.sustained.rate, 10);
    assert_eq!(effective.algorithm, RateAlgorithm::TokenBucket);
}

/// The control-plane aggregate stores a route with the `tenant_id` the caller
/// supplied: unlike the management aggregate it does not server-assign it from
/// the actor. The divergence is pinned here because the management surface
/// (`RouteManagementService`) is the one the REST handlers drive, and it
/// stamps `actor.tenant_id` over whatever the caller passed.
#[tokio::test]
async fn the_control_plane_stores_the_caller_supplied_tenant_identifier() {
    let (tenant, service) = service_of().await;
    let created = service
        .create_upstream(tenant, upstream(tenant, "payments"))
        .expect("the upstream is created");
    let mut record = route(created.id, "/v1/orders");
    record.id = uuid::Uuid::new_v4();
    record.tenant_id = uuid::Uuid::nil();

    let stored = service.create_route(tenant, record).expect("the route is stored");
    assert_eq!(stored.tenant_id, uuid::Uuid::nil(), "no server-side stamping");

    // A route stored under the nil owner is foreign to every tenant merge, so
    // its private rate-limit block contributes nothing to the resolution.
    let target = service
        .resolve_proxy_target(tenant, "payments", "GET", "/v1/orders")
        .expect("the route still resolves");
    assert!(target.upstream.rate_limit.is_none(), "the foreign layer is dropped");
}

/// The upstream-management and route-management surfaces of the same aggregate
/// see the records the resolution sees, so no separate store is in play.
#[tokio::test]
async fn the_resolution_agrees_with_the_management_surfaces() {
    let (tenant, service) = service_of().await;
    let created = service
        .create_upstream(tenant, upstream(tenant, "payments"))
        .expect("the upstream is created");
    service
        .create_route(tenant, route(created.id, "/v1/orders"))
        .expect("the route is created");

    let by_alias = service
        .get_upstream_by_alias(tenant, "payments")
        .expect("the alias resolves");
    assert_eq!(by_alias.id, created.id);
    let routes = service
        .list_routes_for_upstream(tenant, created.id)
        .expect("the routes list");
    assert_eq!(routes.len(), 1);
}
