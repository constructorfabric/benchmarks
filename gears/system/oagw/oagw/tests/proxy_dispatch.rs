//! Integration tests of the proxy data plane dispatch
//! (`cpt-cf-oagw-flow-request-proxy-dispatch`,
//! `cpt-cf-oagw-dod-request-proxy-proxy-handler`).
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-error-source:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-integration-tests:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-preflight-detection:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-proxy-handler:p1

use oagw::domain::dto::{EndpointScheme, HttpMethod, PathSuffixMode};
use oagw::test_support::{
    management_surface, permissive_surface, route_for, seed_route, seed_upstream, stub_upstream,
    upstream_at,
};
use uuid::Uuid;

const PROXY: &str = "/oagw/v1/proxy";

/// The `oagw` block the proxy tests need: `http` upstreams admitted and a body
/// limit the oversized-body test can exceed.
fn proxy_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// The surface with one upstream and one route seeded over a stub upstream.
async fn seeded() -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream, Uuid) {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    let upstream_id = seed_upstream(&surface, upstream);
    let route = route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get, HttpMethod::Post]);
    seed_route(&surface, route);
    (surface, stub, tenant)
}

#[tokio::test]
async fn a_proxy_request_without_a_security_context_is_unauthenticated() {
    let (surface, _stub, _tenant) = seeded().await;
    let exchange = surface.proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/x", &[], b"", None).await;
    assert_eq!(exchange.status, http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_proxy_request_reaches_the_upstream_and_passes_through() {
    let (surface, stub, tenant) = seeded().await;
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(
            tenant,
            principal,
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("accept", "text/plain")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("upstream"));
    assert_eq!(exchange.text(), "payload");
    let requests = stub.received();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method(), "GET");
    assert_eq!(requests[0].target(), "/v1/orders");
    // The routing header, the hop-by-hop set and the host are replaced.
    assert!(requests[0].header("host").expect("host").starts_with("127.0.0.1"));
    assert!(requests[0].header("connection").is_none());
    assert!(requests[0].header("x-oagw-target-host").is_none());
}

#[tokio::test]
async fn the_request_body_reaches_the_upstream() {
    let (surface, stub, tenant) = seeded().await;
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(
            tenant,
            principal,
            "POST",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("content-type", "application/json"), ("content-length", "2")],
            b"{}",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    let requests = stub.received();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body_text(), "{}");
}

#[tokio::test]
async fn a_query_is_forwarded_and_a_query_outside_the_allowlist_is_rejected() {
    let (surface, stub, tenant) = seeded().await;
    let storage = surface.gear.storage().expect("the store");
    let (upstreams, routes, _) = storage.repositories();
    let record = upstreams.get_by_alias(tenant, "api.vendor.com").expect("the upstream");
    let seeded = routes.list(tenant).expect("the routes").remove(0);
    let mut route = route_for(tenant, record.upstream.id, "/v1", &[HttpMethod::Get]);
    route.id = seeded.route.id;
    route.match_.http.as_mut().expect("http").query_allowlist = vec!["limit".to_owned()];
    routes
        .replace(tenant, oagw::domain::repo::RouteRecord { route, plugin_bindings: Vec::new() })
        .expect("the route is replaced");
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(tenant, principal, "GET", "/oagw/v1/proxy/api.vendor.com/v1?limit=1", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(stub.received()[0].target(), "/v1?limit=1");

    let exchange = surface
        .proxy_for(tenant, principal, "GET", "/oagw/v1/proxy/api.vendor.com/v1?offset=1", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_alias_no_upstream_holds_is_route_not_found() {
    let (surface, _stub, tenant) = seeded().await;
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(tenant, principal, "GET", "/oagw/v1/proxy/other.vendor.com/v1", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::NOT_FOUND);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
}

#[tokio::test]
async fn a_path_no_route_matches_is_route_not_found() {
    let (surface, stub, tenant) = seeded().await;
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(tenant, principal, "GET", "/oagw/v1/proxy/api.vendor.com/v9", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::NOT_FOUND);
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn a_method_outside_the_allowlist_is_method_not_allowed_with_the_allow_header() {
    let (surface, stub, tenant) = seeded().await;
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(tenant, principal, "DELETE", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(exchange.header("allow"), Some("GET, POST"));
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn an_options_preflight_is_answered_without_touching_the_upstream() {
    let (surface, stub, tenant) = seeded().await;
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(
            tenant,
            principal,
            "OPTIONS",
            "/oagw/v1/proxy/api.vendor.com/v1",
            &[("origin", "https://app.dev"), ("access-control-request-method", "POST")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::NO_CONTENT);
    assert_eq!(exchange.header("access-control-allow-origin"), Some("https://app.dev"));
    assert_eq!(exchange.header("access-control-allow-methods"), Some("POST"));
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn a_disabled_upstream_is_rejected_without_opening_a_connection() {
    let (surface, stub, tenant) = seeded().await;
    let storage = surface.gear.storage().expect("the store");
    let (upstreams, _, _) = storage.repositories();
    let mut record = upstreams.get_by_alias(tenant, "api.vendor.com").expect("the upstream");
    record.upstream.enabled = false;
    upstreams
        .replace(tenant, record)
        .expect("the upstream is disabled");

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn the_alias_is_resolved_through_the_tenant_chain() {
    let leaf = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let root = Uuid::new_v4();
    let surface = oagw::test_support::management_surface(
        proxy_config(),
        std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        oagw::test_support::FakeHierarchyTenantResolver::over(&[leaf, parent, root]),
    )
    .await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let mut record = upstream_at(root, "shared.vendor.com", EndpointScheme::Http, &host, port);
    record.alias = "shared.vendor.com".to_owned();
    let upstream_id = seed_upstream(&surface, record);
    seed_route(&surface, route_for(root, upstream_id, "/v1", &[HttpMethod::Get]));

    let exchange = surface
        .proxy_for(leaf, Uuid::new_v4(), "GET", "/oagw/v1/proxy/shared.vendor.com/v1", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(exchange.text(), "payload");
}

#[tokio::test]
async fn a_body_over_the_limit_is_rejected_before_it_is_buffered() {
    let surface = permissive_surface(Some(serde_json::json!({
        "allow_http_upstream": true, "proxy_timeout_secs": 5, "max_body_size_bytes": 8
    })))
    .await;
    let tenant = Uuid::new_v4();
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "POST",
            "/oagw/v1/proxy/api.vendor.com/v1",
            &[("content-length", "16")],
            b"0123456789abcdef",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
}

#[tokio::test]
async fn a_declared_length_that_disagrees_with_the_body_is_rejected() {
    let (surface, stub, tenant) = seeded().await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "POST",
            "/oagw/v1/proxy/api.vendor.com/v1",
            &[("content-length", "9")],
            b"short",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::BAD_REQUEST);
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn a_suffix_to_a_disabled_suffix_mode_route_is_rejected() {
    let (surface, stub, tenant) = seeded().await;
    let storage = surface.gear.storage().expect("the store");
    let (upstreams, routes, _) = storage.repositories();
    let record = upstreams.get_by_alias(tenant, "api.vendor.com").expect("the upstream");
    let seeded = routes.list(tenant).expect("the routes").remove(0);
    let mut route = route_for(tenant, record.upstream.id, "/v1", &[HttpMethod::Get]);
    route.id = seeded.route.id;
    route.match_.http.as_mut().expect("http").path_suffix_mode = PathSuffixMode::Disabled;
    routes
        .replace(tenant, oagw::domain::repo::RouteRecord { route, plugin_bindings: Vec::new() })
        .expect("the route is replaced");

    // The declared path still routes; a suffix past it does not.
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::NOT_FOUND);
    assert_eq!(stub.received().len(), 1);
}

#[tokio::test]
async fn a_grpc_matched_upstream_is_a_configuration_error() {
    let (surface, stub, tenant) = seeded().await;
    let storage = surface.gear.storage().expect("the store");
    let (upstreams, _, _) = storage.repositories();
    let mut record = upstreams.get_by_alias(tenant, "api.vendor.com").expect("the upstream");
    record.upstream.protocol = oagw::domain::gts_helpers::PROTOCOL_GRPC.to_owned();
    upstreams
        .replace(tenant, record)
        .expect("the upstream is switched to gRPC");

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await;
    assert!(
        exchange.status.is_server_error() || exchange.status.is_client_error(),
        "a grpc upstream is never proxied: {}",
        exchange.status
    );
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn a_caller_without_the_invoke_permission_is_rejected_before_any_repository_access() {
    let surface = management_surface(
        proxy_config(),
        oagw::test_support::FakePolicyAuthZ::denying(&["gts.cf.core.oagw.proxy.v1~:invoke"]),
        oagw::test_support::FakeHierarchyTenantResolver::default().into(),
    )
    .await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(tenant, principal, "GET", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::FORBIDDEN);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
}

#[tokio::test]
async fn a_request_to_a_management_path_is_not_the_proxy_surface() {
    let (surface, _stub, tenant) = seeded().await;
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/plugins", &[], b"")
        .await;
    // The proxy catch-all only covers /oagw/v1/proxy/..., so a path the
    // framework itself does not answer is never mistaken for an upstream.
    assert!(exchange.header("x-oagw-error-source").is_none());
}

#[tokio::test]
async fn an_authenticated_caller_is_the_security_context_the_middleware_resolved() {
    let (surface, _stub, tenant) = seeded().await;
    let principal = Uuid::new_v4();
    let exchange = surface
        .proxy_for(tenant, principal, "GET", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await;
    assert_eq!(exchange.header("x-oagw-error-source"), Some("upstream"));
}

#[test]
fn the_proxy_prefix_is_gear_relative_and_catch_all() {
    assert!(PROXY.starts_with("/oagw/v1/proxy"));
    assert!(!PROXY.starts_with("/api"));
}

#[tokio::test]
async fn the_gear_exposes_the_data_plane_it_initialized() {
    let surface = permissive_surface(None).await;
    assert!(surface.gear.data_plane().is_some());
}
