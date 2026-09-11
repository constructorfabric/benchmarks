//! Integration tests of the plugin chain composition and ordering against the
//! proxy pipeline (`cpt-cf-oagw-dod-plugin-system-chain-ordering`).
//!
//! The composition is a pure concatenation, so the *order* itself is asserted
//! through the two steps the proxy pipeline runs — `compose` and then
//! `PluginRuntime::resolve` — over the very binding rows the pipeline reads
//! from the store. The proxied request in the same test proves the composed
//! chain actually executes: the built-in `request_id` transform is the only
//! writer of `X-Request-ID`, so its presence on what the stub upstream
//! receives is a positive marker that a chain entry ran.
//!
//! The ancestor cases are decisive black-box probes: an ancestor binding under
//! `sharing: enforce` reaches the descendant's chain and is executed, and the
//! same binding under `sharing: private` never does.

use std::sync::Arc;

use oagw::domain::dto::{EndpointScheme, HttpMethod, PluginsConfig, SharingMode};
use oagw::domain::plugin::composition::{compose, ChainLayer};
use oagw::domain::repo::{PluginBinding, RouteRecord, UpstreamRecord};
use oagw::infra::plugin::executor::PluginRuntime;
use oagw::infra::plugin::resolution::{PluginRegistries, TenantChain};
use oagw::test_support::{
    FakeCredStore, ManagementSurface, permissive_surface, route_for, upstream_at,
};
use uuid::Uuid;

const REQUEST_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
const APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
const REQUIRED_HEADERS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

fn proxy_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

fn binding(position: u32, reference: &str) -> PluginBinding {
    PluginBinding { position, plugin_ref: reference.to_owned(), plugin_uuid: None }
}

/// Persist an upstream carrying `references` as its ordered plugin bindings.
fn seed_upstream_with(
    surface: &ManagementSurface,
    record: oagw::domain::dto::Upstream,
    references: &[&str],
) -> Uuid {
    let bindings = references
        .iter()
        .enumerate()
        .map(|(position, reference)| binding(position as u32, reference))
        .collect();
    let storage = surface.gear.storage().expect("the store is initialized");
    let (upstreams, _, _) = storage.repositories();
    upstreams
        .create(
            record.tenant_id,
            UpstreamRecord { upstream: record.clone(), plugin_bindings: bindings },
        )
        .expect("the upstream is seeded");
    record.id
}

/// Persist a route carrying `references` as its ordered plugin bindings.
fn seed_route_with(
    surface: &ManagementSurface,
    route: oagw::domain::dto::Route,
    references: &[&str],
) -> Uuid {
    let bindings = references
        .iter()
        .enumerate()
        .map(|(position, reference)| binding(position as u32, reference))
        .collect();
    let storage = surface.gear.storage().expect("the store is initialized");
    let (_, routes, _) = storage.repositories();
    routes
        .create(route.tenant_id, RouteRecord { route: route.clone(), plugin_bindings: bindings })
        .expect("the route is seeded");
    route.id
}

/// An upstream under `alias` pointing at `stub`, carrying `plugins`.
fn level(
    tenant: Uuid,
    alias: &str,
    host: &str,
    port: u16,
    sharing: SharingMode,
    references: &[&str],
) -> oagw::domain::dto::Upstream {
    let mut record = upstream_at(tenant, alias, EndpointScheme::Http, host, port);
    record.plugins = Some(PluginsConfig {
        sharing,
        items: references.iter().map(|reference| (*reference).to_owned()).collect(),
    });
    record
}

/// The runtime the gear itself builds, over the same built-in registries.
fn runtime(storage: &Arc<oagw::infra::storage::Storage>) -> PluginRuntime {
    let (_, _, plugins) = storage.repositories();
    PluginRuntime::new(
        Arc::new(PluginRegistries::with_builtins(
            Arc::new(FakeCredStore),
            oagw::TokenCacheConfig::default(),
            plugins,
        )),
        5,
    )
}

/// The resolved entry references of one composed chain, in execution order.
fn resolved_order(
    runtime: &PluginRuntime,
    ancestors: &[ChainLayer],
    upstream: &[PluginBinding],
    route: &[PluginBinding],
) -> Vec<String> {
    let composed = compose(ancestors, upstream, route, None).expect("the chain composes");
    let chain = runtime
        .resolve(&composed, &TenantChain::default(), None)
        .expect("every reference resolves");
    chain.entries.iter().map(|entry| entry.reference().to_owned()).collect()
}

#[tokio::test]
async fn the_upstream_level_precedes_the_route_level() {
    let surface = permissive_surface(proxy_config()).await;
    let stub = oagw::test_support::stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream_id = seed_upstream_with(
        &surface,
        upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port),
        &[REQUEST_ID, REQUIRED_HEADERS],
    );
    seed_route_with(
        &surface,
        route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]),
        &[REQUIRED_HEADERS, REQUEST_ID],
    );

    // The composed chain executes: the request reaches the upstream and the
    // request-id transform has written the only `X-Request-ID` header.
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{:?}'", exchange.text());
    let received = stub.received();
    assert_eq!(received.len(), 1);
    assert!(
        received[0].header("x-request-id").is_some(),
        "the request-id transform of the composed chain ran: {received:?}"
    );

    // The order the chain ran in is the order the two levels compose to, read
    // from the same rows the proxy read them from.
    let storage = surface.gear.storage().expect("the store");
    let (upstreams, routes, _) = storage.repositories();
    let upstream = upstreams.get(tenant, upstream_id).expect("the upstream").plugin_bindings;
    let route = routes
        .list(tenant)
        .expect("the routes")
        .remove(0)
        .plugin_bindings;
    let runtime = runtime(&storage);
    assert_eq!(
        resolved_order(&runtime, &[], &upstream, &route),
        vec![
            REQUEST_ID.to_owned(),
            REQUIRED_HEADERS.to_owned(),
            REQUIRED_HEADERS.to_owned(),
            REQUEST_ID.to_owned(),
        ],
        "[U1, U2] + [R1, R2] => [U1, U2, R1, R2]"
    );
}

#[tokio::test]
async fn an_enforced_ancestor_binding_survives_in_a_descendants_chain() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let surface = oagw::test_support::management_surface(
        proxy_config(),
        Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        oagw::test_support::FakeHierarchyTenantResolver::over(&[child, parent]),
    )
    .await;
    let stub = oagw::test_support::stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();

    // The ancestor bound the transform under `sharing: enforce`.
    seed_upstream_with(
        &surface,
        level(parent, "api.vendor.com", &host, port, SharingMode::Enforce, &[REQUEST_ID]),
        &[REQUEST_ID],
    );
    // The descendant owns the record the walk selects.
    let upstream_id =
        seed_upstream_with(&surface, upstream_at(child, "api.vendor.com", EndpointScheme::Http, &host, port), &[]);
    seed_route_with(&surface, route_for(child, upstream_id, "/v1", &[HttpMethod::Get]), &[]);

    let exchange = surface
        .proxy_for(child, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
    let received = stub.received();
    assert_eq!(received.len(), 1);
    assert!(
        received[0].header("x-request-id").is_some(),
        "the enforced ancestor binding is retained and executed: {received:?}"
    );
}

#[tokio::test]
async fn a_private_ancestor_binding_is_absent_from_a_descendants_chain() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let surface = oagw::test_support::management_surface(
        proxy_config(),
        Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        oagw::test_support::FakeHierarchyTenantResolver::over(&[child, parent]),
    )
    .await;
    let stub = oagw::test_support::stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();

    // The same binding, this time `private`: the descendant sees none of it.
    seed_upstream_with(
        &surface,
        level(parent, "api.vendor.com", &host, port, SharingMode::Private, &[REQUEST_ID]),
        &[REQUEST_ID],
    );
    let upstream_id =
        seed_upstream_with(&surface, upstream_at(child, "api.vendor.com", EndpointScheme::Http, &host, port), &[]);
    seed_route_with(&surface, route_for(child, upstream_id, "/v1", &[HttpMethod::Get]), &[]);

    let exchange = surface
        .proxy_for(child, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
    let received = stub.received();
    assert_eq!(received.len(), 1);
    assert!(
        received[0].header("x-request-id").is_none(),
        "a private ancestor binding contributes nothing: {received:?}"
    );
}

/// An auth plugin is never taken from the chain: a chain position that names
/// one fails the request rather than authenticating from the chain.
#[tokio::test]
async fn an_auth_plugin_in_a_chain_position_fails_the_request() {
    let surface = permissive_surface(proxy_config()).await;
    let stub = oagw::test_support::stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream_id = seed_upstream_with(
        &surface,
        upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port),
        &[REQUEST_ID, APIKEY],
    );
    seed_route_with(
        &surface,
        route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]),
        &[REQUEST_ID],
    );

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::SERVICE_UNAVAILABLE, "{:?}", exchange.text());
    assert!(
        exchange.text().contains("plugin.not_found"),
        "the failure names the plugin reference: {:?}",
        exchange.text()
    );
    assert!(
        stub.received().is_empty(),
        "the first rejection prevents the upstream call"
    );
}

/// `inst-rp-chain-5`/`-6`: the bindings of a level are read under the tenant
/// that owns the level. When the walk selects an *ancestor's* upstream and the
/// route matched is the ancestor's too, a read keyed by the caller's tenant
/// would find nothing and the binding the ancestor configured would silently
/// drop out of the descendant's chain.
#[tokio::test]
async fn an_ancestor_owned_selected_upstream_keeps_its_bindings() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let surface = oagw::test_support::management_surface(
        proxy_config(),
        Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        oagw::test_support::FakeHierarchyTenantResolver::over(&[child, parent]),
    )
    .await;
    let stub = oagw::test_support::stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();

    // The ancestor owns the upstream the walk selects, and it binds the
    // request-id transform under `sharing: enforce`.
    let upstream_id = seed_upstream_with(
        &surface,
        level(parent, "api.vendor.com", &host, port, SharingMode::Enforce, &[REQUEST_ID]),
        &[REQUEST_ID],
    );
    seed_route_with(&surface, route_for(parent, upstream_id, "/v1", &[HttpMethod::Get]), &[]);

    // The descendant holds neither the alias nor the route: the whole chain is
    // the ancestor's, and its binding still runs.
    let exchange = surface
        .proxy_for(child, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.text());
    let received = stub.received();
    assert_eq!(received.len(), 1);
    assert!(
        received[0].header("x-request-id").is_some(),
        "the ancestor's binding ran for the descendant's request: {received:?}"
    );
}
