use std::sync::Arc;

use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::DataPlaneService;
use crate::domain::error::ErrorKind;
use crate::domain::model::{
    Endpoint, HttpMatch, PathSuffixMode, Route, RouteMatch, RouteSpec, Scheme, ServerConfig,
    Upstream, UpstreamSpec,
};
use crate::domain::repo::{RouteRepository, UpstreamRepository};
use crate::infra::storage::memory::MemoryStore;

/// Deterministic tenants: `parent` is the root of `child`.
static PARENT: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00a1);

fn context(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(9))
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"))
}

fn upstream(tenant: Uuid, alias: &str, host: &str, enabled: bool) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        alias_explicit: false,
        spec: UpstreamSpec {
            enabled,
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: host.to_owned(),
                    port: 443,
                }],
            },
            ..UpstreamSpec::default()
        },
        created_at: 0,
        updated_at: 0,
    }
}

fn route(tenant: Uuid, upstream_id: Uuid, pattern: &str) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id,
        spec: RouteSpec {
            r#match: RouteMatch::Http(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: pattern.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            ..RouteSpec::default()
        },
        created_at: 0,
        updated_at: 0,
    }
}

/// The store bound through each repository trait, so the identically named
/// methods stay unambiguous at the call sites.
struct Fixtures {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
}

fn fixtures() -> (Arc<MemoryStore>, Fixtures) {
    let store = MemoryStore::new();
    let fixtures = Fixtures {
        upstreams: store.clone(),
        routes: store.clone(),
    };
    (store, fixtures)
}

fn insert_upstream(fixtures: &Fixtures, upstream: Upstream) {
    fixtures
        .upstreams
        .insert(upstream)
        .unwrap_or_else(|error| panic!("insert upstream: {error}"));
}

fn insert_route(fixtures: &Fixtures, route: Route) {
    fixtures
        .routes
        .insert(route)
        .unwrap_or_else(|error| panic!("insert route: {error}"));
}

/// A service with no tenant resolver: the chain is the caller's own tenant.
fn service(store: &Arc<MemoryStore>) -> DataPlaneService {
    DataPlaneService::new(store.clone(), store.clone(), None)
}

fn deterministic_id(seed: u128) -> Uuid {
    Uuid::from_u128(seed)
}

#[tokio::test]
async fn the_callers_own_tenant_is_searched_first() {
    let (store, fixtures) = fixtures();
    insert_upstream(
        &fixtures,
        upstream(PARENT, "billing", "billing.example.com", true),
    );
    let service = service(&store);
    let resolution = service
        .resolve(&context(PARENT), "billing", "GET", "/orders", &[], None)
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    assert_eq!(resolution.upstream.alias, "billing");
    assert_eq!(resolution.endpoint.host, "billing.example.com");
    assert!(resolution.route.is_none(), "no routes are configured yet");
}

#[test]
fn ids_are_formed_from_the_type_and_instance() {
    let id = deterministic_id(1);
    let rendered = crate::ids::format_id(crate::ids::UPSTREAM_TYPE, id);
    assert!(rendered.starts_with(crate::ids::UPSTREAM_TYPE));
    assert_eq!(crate::ids::instance_part(&rendered), id.to_string());
}

#[tokio::test]
async fn a_disabled_upstream_is_not_served() {
    let (store, fixtures) = fixtures();
    insert_upstream(
        &fixtures,
        upstream(PARENT, "billing", "billing.example.com", false),
    );
    let service = service(&store);
    let error = service
        .resolve(&context(PARENT), "billing", "GET", "/orders", &[], None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::LinkUnavailable);
}

#[tokio::test]
async fn an_unknown_alias_is_not_found() {
    let (store, _fixtures) = fixtures();
    let service = service(&store);
    let error = service
        .resolve(&context(PARENT), "absent", "GET", "/", &[], None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::ResourceNotFound);
}

#[tokio::test]
async fn aliases_are_normalized_before_lookup() {
    let (store, fixtures) = fixtures();
    // Mixed-case spelling is canonicalized on entry (DESIGN.md: "Aliases are
    // normalized to ASCII lowercase with trailing dots stripped"), and the
    // resolution of any case variant reaches the same record.
    insert_upstream(
        &fixtures,
        upstream(PARENT, "Billing.Service.", "b.example.com", true),
    );
    let service = service(&store);
    let resolution = service
        .resolve(&context(PARENT), "billing.service", "GET", "/", &[], None)
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    assert_eq!(resolution.upstream.alias, "billing.service");
}

#[tokio::test]
async fn a_route_is_matched_and_its_path_rewritten() {
    let (store, fixtures) = fixtures();
    let mut owned = upstream(PARENT, "billing", "billing.example.com", true);
    owned.id = deterministic_id(0x11);
    insert_upstream(&fixtures, owned);
    insert_route(&fixtures, route(PARENT, deterministic_id(0x11), "/orders"));
    let service = service(&store);
    let resolution = service
        .resolve(&context(PARENT), "billing", "GET", "/orders/42", &[], None)
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    let matched = resolution
        .route
        .unwrap_or_else(|| panic!("a route must match `/orders/42`"));
    assert_eq!(
        matched.spec.r#match,
        RouteMatch::Http(HttpMatch {
            methods: vec!["GET".to_owned()],
            path: "/orders".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        })
    );
    assert_eq!(resolution.path, "/orders/42");
    assert!(resolution.effective.enabled);
}

#[tokio::test]
async fn an_unmatched_path_keeps_the_request_path() {
    let (store, fixtures) = fixtures();
    let mut owned = upstream(PARENT, "billing", "billing.example.com", true);
    owned.id = deterministic_id(0x11);
    insert_upstream(&fixtures, owned);
    let service = service(&store);
    let resolution = service
        .resolve(&context(PARENT), "billing", "GET", "/anything", &[], None)
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    assert!(resolution.route.is_none());
    assert_eq!(resolution.path, "/anything");
}

#[tokio::test]
async fn a_declared_route_that_matches_nothing_is_a_404() {
    let (store, fixtures) = fixtures();
    let mut owned = upstream(PARENT, "billing", "billing.example.com", true);
    owned.id = deterministic_id(0x11);
    insert_upstream(&fixtures, owned);
    insert_route(&fixtures, route(PARENT, deterministic_id(0x11), "/orders"));
    let service = service(&store);
    let error = service
        .resolve(&context(PARENT), "billing", "GET", "/anything", &[], None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::RouteNotFound);
    assert_eq!(error.kind.http_status(), 404);
}

#[tokio::test]
async fn the_target_host_selects_within_the_pool() {
    let (store, fixtures) = fixtures();
    let mut owned = upstream(PARENT, "sharded", "a.example.com", true);
    owned.id = deterministic_id(0x12);
    owned.alias_explicit = true;
    owned.spec.server.endpoints = vec![
        Endpoint {
            scheme: Scheme::Https,
            host: "a.example.com".to_owned(),
            port: 443,
        },
        Endpoint {
            scheme: Scheme::Https,
            host: "b.example.com".to_owned(),
            port: 443,
        },
    ];
    insert_upstream(&fixtures, owned);
    let service = service(&store);
    let error = service
        .resolve(&context(PARENT), "sharded", "GET", "/", &[], None)
        .await
        .unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::MissingTargetHost,
        "a shared suffix pool needs a target host"
    );
    let resolution = service
        .resolve(
            &context(PARENT),
            "sharded",
            "GET",
            "/",
            &[],
            Some("b.example.com"),
        )
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    assert_eq!(resolution.endpoint.host, "b.example.com");
}

#[tokio::test]
async fn the_service_keeps_a_stable_round_robin() {
    let (store, fixtures) = fixtures();
    let mut owned = upstream(PARENT, "pooled", "a.example.com", true);
    owned.spec.server.endpoints = vec![
        Endpoint {
            scheme: Scheme::Https,
            host: "a.example.com".to_owned(),
            port: 443,
        },
        Endpoint {
            scheme: Scheme::Https,
            host: "b.other.org".to_owned(),
            port: 443,
        },
    ];
    insert_upstream(&fixtures, owned);
    let service = service(&store);
    let first = service
        .resolve(&context(PARENT), "pooled", "GET", "/", &[], None)
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    let second = service
        .resolve(&context(PARENT), "pooled", "GET", "/", &[], None)
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    assert_ne!(first.endpoint.host, second.endpoint.host);
}

#[test]
fn a_service_can_be_built_without_a_tenant_resolver() {
    let (_store, fixtures) = fixtures();
    let service = DataPlaneService::new(fixtures.upstreams, fixtures.routes, None);
    let _ = Arc::new(service);
}
