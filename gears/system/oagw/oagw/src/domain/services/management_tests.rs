//! `ControlPlaneService` tests: CRUD, alias transitions, tenant scoping and the
//! single resolution walk the data plane depends on.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{
    Endpoint, EndpointScheme, HttpMethod, MatchConfig, PluginBinding, RateLimitConfig,
    SustainedRate, Upstream,
};
use crate::domain::repo::TenantHierarchy;
use crate::domain::services::management::{
    ControlPlaneService, ProxyMethod, best_matching_route, check_scheme_admission, validate_cors,
    validate_plugin_binding,
};
use crate::infra::storage::memory::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
};

const TENANT: &str = "00000000-0000-0000-0000-000000000001";
const OTHER: &str = "00000000-0000-0000-0000-000000000002";

fn service() -> ControlPlaneService {
    ControlPlaneService::new(
        Arc::new(MemoryUpstreamRepository::default()),
        Arc::new(MemoryRouteRepository::default()),
        Arc::new(MemoryPluginRepository::default()),
        Arc::new(FlatHierarchy),
        true,
    )
}

/// A hierarchy that answers with the tenant itself, so resolution has no
/// ancestors to walk.
struct FlatHierarchy;

#[async_trait::async_trait]
impl TenantHierarchy for FlatHierarchy {
    async fn chain(&self, tenant_id: &str) -> Vec<String> {
        vec![tenant_id.to_owned()]
    }
}

fn host_upstream(host: &str, port: u16) -> Upstream {
    Upstream {
        enabled: true,
        server: crate::domain::model::ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: host.to_owned(),
                port: Some(port),
            }],
        },
        protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
        ..Default::default()
    }
}

fn http_match(path: &str, methods: &[HttpMethod]) -> MatchConfig {
    MatchConfig {
        http: Some(crate::domain::model::HttpMatch {
            methods: methods.to_vec(),
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

async fn upstream_with_route(
    path: &str,
    methods: &[HttpMethod],
) -> (ControlPlaneService, Upstream) {
    let svc = service();
    let upstream = svc
        .create_upstream(TENANT, host_upstream("api.example.com", 80), None)
        .await
        .unwrap();
    svc.create_route(TENANT, route_for(upstream.id, path, methods))
        .await
        .unwrap();
    (svc, upstream)
}

/// An enabled HTTP route for `upstream`.
fn route_for(upstream_id: Uuid, path: &str, methods: &[HttpMethod]) -> crate::domain::model::Route {
    crate::domain::model::Route {
        upstream_id,
        enabled: true,
        match_config: http_match(path, methods),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_upstream_derives_alias_from_hostname() {
    let svc = service();
    let upstream = svc
        .create_upstream(TENANT, host_upstream("api.openai.com", 80), None)
        .await
        .unwrap();
    assert_eq!(upstream.alias, "api.openai.com");
    assert!(upstream.enabled);
    assert_eq!(upstream.tenant_id, TENANT);
    assert!(upstream.gts_id.starts_with("gts.cf.core.oagw.upstream.v1~"));
    assert!(!upstream.created_at.is_empty());
    assert_eq!(upstream.created_at, upstream.updated_at);
}

#[tokio::test]
async fn derived_alias_keeps_a_non_standard_port() {
    let svc = service();
    let upstream = svc
        .create_upstream(TENANT, host_upstream("api.openai.com", 8443), None)
        .await
        .unwrap();
    assert_eq!(upstream.alias, "api.openai.com:8443");
}

#[tokio::test]
async fn provided_alias_must_match_the_derivation() {
    let svc = service();
    let err = svc
        .create_upstream(
            TENANT,
            host_upstream("api.openai.com", 80),
            Some("other.example.com".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Validation(_)), "{err:?}");
}

#[tokio::test]
async fn ip_endpoints_require_an_explicit_alias() {
    let svc = service();
    let err = svc
        .create_upstream(TENANT, host_upstream("127.0.0.1", 9000), None)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Validation(_)), "{err:?}");

    let created = svc
        .create_upstream(
            TENANT,
            host_upstream("127.0.0.1", 9000),
            Some("Local-Service".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(created.alias, "local-service");
}

#[tokio::test]
async fn pool_with_common_suffix_derives_the_suffix() {
    let svc = service();
    let mut upstream = host_upstream("api.openai.com", 80);
    upstream.server.endpoints.push(Endpoint {
        scheme: EndpointScheme::Http,
        host: "backup.openai.com".to_owned(),
        port: Some(80),
    });
    let created = svc.create_upstream(TENANT, upstream, None).await.unwrap();
    assert_eq!(created.alias, "openai.com");
}

#[tokio::test]
async fn duplicate_alias_conflicts() {
    let svc = service();
    svc.create_upstream(TENANT, host_upstream("api.openai.com", 80), None)
        .await
        .unwrap();
    let err = svc
        .create_upstream(TENANT, host_upstream("api.openai.com", 80), None)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Conflict(_)), "{err:?}");
}

#[tokio::test]
async fn upstreams_are_tenant_scoped() {
    let svc = service();
    let created = svc
        .create_upstream(TENANT, host_upstream("api.openai.com", 80), None)
        .await
        .unwrap();
    assert!(svc.get_upstream(OTHER, created.id).await.is_err());
    assert!(svc.list_upstreams(OTHER).await.unwrap().is_empty());
    // The same alias may exist in another tenant.
    svc.create_upstream(OTHER, host_upstream("api.openai.com", 80), None)
        .await
        .unwrap();
    assert_eq!(svc.list_upstreams(OTHER).await.unwrap().len(), 1);
}

#[tokio::test]
async fn replace_keeps_the_derived_alias_stable() {
    let svc = service();
    let created = svc
        .create_upstream(TENANT, host_upstream("api.openai.com", 80), None)
        .await
        .unwrap();

    // Same endpoints, different port: the alias would change, so it is refused.
    let mut replacement = host_upstream("api.openai.com", 9443);
    replacement.id = created.id;
    let err = svc
        .replace_upstream(TENANT, created.id, replacement, None)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Validation(_)), "{err:?}");

    // Idempotent replacement keeps the alias and refreshes `updated_at`.
    let mut same = host_upstream("api.openai.com", 80);
    same.id = created.id;
    same.tags = vec!["stable".to_owned()];
    let replaced = svc
        .replace_upstream(TENANT, created.id, same, None)
        .await
        .unwrap();
    assert_eq!(replaced.alias, "api.openai.com");
    assert_eq!(replaced.tags, vec!["stable".to_owned()]);
}

#[tokio::test]
async fn replace_cannot_rename_an_explicit_alias() {
    let svc = service();
    let created = svc
        .create_upstream(
            TENANT,
            host_upstream("127.0.0.1", 9000),
            Some("local".to_owned()),
        )
        .await
        .unwrap();
    let mut replacement = host_upstream("127.0.0.1", 9000);
    replacement.id = created.id;
    let err = svc
        .replace_upstream(TENANT, created.id, replacement, Some("renamed".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Validation(_)), "{err:?}");
}

#[tokio::test]
async fn delete_upstream_cascades_its_routes() {
    let (svc, upstream) = upstream_with_route("/v1", &[HttpMethod::Get]).await;
    assert_eq!(svc.list_routes(TENANT).await.unwrap().len(), 1);
    svc.delete_upstream(TENANT, upstream.id).await.unwrap();
    assert!(svc.list_upstreams(TENANT).await.unwrap().is_empty());
    assert!(svc.list_routes(TENANT).await.unwrap().is_empty());
    assert!(matches!(
        svc.delete_upstream(TENANT, upstream.id).await.unwrap_err(),
        DomainError::NotFound(_)
    ));
}

#[tokio::test]
async fn unknown_upstream_is_not_found() {
    let svc = service();
    assert!(matches!(
        svc.get_upstream(TENANT, Uuid::new_v4()).await.unwrap_err(),
        DomainError::NotFound(_)
    ));
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_requires_a_known_upstream() {
    let svc = service();
    let route = crate::domain::model::Route {
        upstream_id: Uuid::new_v4(),
        match_config: http_match("/", &[HttpMethod::Get]),
        ..Default::default()
    };
    assert!(matches!(
        svc.create_route(TENANT, route).await.unwrap_err(),
        DomainError::NotFound(_)
    ));
}

#[tokio::test]
async fn route_match_must_name_exactly_one_protocol() {
    let (svc, upstream) = upstream_with_route("/v1", &[HttpMethod::Get]).await;

    let mut both = route_for(upstream.id, "/both", &[HttpMethod::Get]);
    both.match_config.grpc = Some(crate::domain::model::GrpcMatch {
        service: "svc.S".to_owned(),
        method: "M".to_owned(),
    });
    assert!(matches!(
        svc.create_route(TENANT, both).await.unwrap_err(),
        DomainError::Validation(_)
    ));

    let mut none = route_for(upstream.id, "/none", &[HttpMethod::Get]);
    none.match_config.http = None;
    assert!(matches!(
        svc.create_route(TENANT, none).await.unwrap_err(),
        DomainError::Validation(_)
    ));

    // A gRPC match is storable, but no HTTP request ever matches it.
    let mut grpc = route_for(upstream.id, "/grpc", &[HttpMethod::Get]);
    grpc.match_config.http = None;
    grpc.match_config.grpc = Some(crate::domain::model::GrpcMatch {
        service: "svc.S".to_owned(),
        method: "M".to_owned(),
    });
    let stored = svc.create_route(TENANT, grpc).await.unwrap();
    assert!(stored.match_config.grpc.is_some());
}

#[tokio::test]
async fn duplicate_route_match_conflicts() {
    let (svc, upstream) = upstream_with_route("/v1", &[HttpMethod::Get]).await;
    let route = route_for(upstream.id, "/v1", &[HttpMethod::Get]);
    assert!(matches!(
        svc.create_route(TENANT, route).await.unwrap_err(),
        DomainError::Conflict(_)
    ));
    // A different method on the same path is a different route.
    let other = route_for(upstream.id, "/v1", &[HttpMethod::Post]);
    assert!(svc.create_route(TENANT, other).await.is_ok());
    // A new disabled route still conflicts with the live one.
    let mut disabled = route_for(upstream.id, "/v1", &[HttpMethod::Get]);
    disabled.enabled = false;
    assert!(matches!(
        svc.create_route(TENANT, disabled).await.unwrap_err(),
        DomainError::Conflict(_)
    ));
}

#[tokio::test]
async fn route_upstream_id_is_immutable() {
    let (svc, upstream) = upstream_with_route("/v1", &[HttpMethod::Get]).await;
    let other = svc
        .create_upstream(TENANT, host_upstream("backup.openai.com", 80), None)
        .await
        .unwrap();
    let existing = svc.list_routes(TENANT).await.unwrap().remove(0);

    // Repointing the route at another upstream is refused outright.
    let repointed = route_for(other.id, "/v2", &[HttpMethod::Get]);
    assert!(matches!(
        svc.replace_route(TENANT, existing.id, repointed)
            .await
            .unwrap_err(),
        DomainError::Validation(_)
    ));

    // A replacement carrying the original `upstream_id` is accepted.
    let replacement = route_for(upstream.id, "/v2", &[HttpMethod::Get]);
    let replaced = svc
        .replace_route(TENANT, existing.id, replacement)
        .await
        .unwrap();
    assert_eq!(replaced.upstream_id, upstream.id);
    assert_eq!(replaced.match_config, http_match("/v2", &[HttpMethod::Get]));
}

#[tokio::test]
async fn disabled_routes_are_stored_and_listed() {
    let (svc, upstream) = upstream_with_route("/v1", &[HttpMethod::Get]).await;
    let mut route = route_for(upstream.id, "/v2", &[HttpMethod::Get]);
    route.enabled = false;
    let created = svc.create_route(TENANT, route).await.unwrap();
    assert!(!created.enabled);
    assert_eq!(svc.list_routes(TENANT).await.unwrap().len(), 2);
    // Disabled routes are skipped by matching.
    assert!(
        best_matching_route(
            &svc.list_routes(TENANT).await.unwrap(),
            ProxyMethod::Get,
            "/v2"
        )
        .is_none()
    );
}

// ---------------------------------------------------------------------------
// Route matching + resolution
// ---------------------------------------------------------------------------

#[test]
fn longest_prefix_wins() {
    let routes: Vec<crate::domain::model::Route> = ["/v1", "/v1/chat", "/v1/chat/completions"]
        .iter()
        .map(|path| crate::domain::model::Route {
            enabled: true,
            match_config: http_match(path, &[HttpMethod::Get]),
            ..Default::default()
        })
        .collect();

    let (route, suffix) =
        best_matching_route(&routes, ProxyMethod::Get, "/v1/chat/completions/now").unwrap();
    assert_eq!(
        route.match_config.http.unwrap().path,
        "/v1/chat/completions"
    );
    assert_eq!(suffix, "/now");

    let (route, suffix) = best_matching_route(&routes, ProxyMethod::Get, "/v1/chat").unwrap();
    assert_eq!(route.match_config.http.unwrap().path, "/v1/chat");
    assert!(suffix.is_empty());

    // A prefix must end on a segment boundary.
    assert!(best_matching_route(&routes, ProxyMethod::Get, "/v1chat").is_none());
}

#[test]
fn a_route_admitting_the_method_is_preferred_over_one_that_does_not() {
    let routes = vec![
        crate::domain::model::Route {
            enabled: true,
            match_config: http_match("/v1", &[HttpMethod::Get]),
            ..Default::default()
        },
        crate::domain::model::Route {
            enabled: true,
            match_config: http_match("/v1", &[HttpMethod::Post]),
            ..Default::default()
        },
    ];
    // Sibling routes splitting one path by verb each keep their own traffic.
    let (route, _) = best_matching_route(&routes, ProxyMethod::Post, "/v1").unwrap();
    assert_eq!(
        route.match_config.http.unwrap().methods,
        vec![HttpMethod::Post]
    );
}

#[test]
fn a_method_no_route_admits_still_resolves_to_the_path_match() {
    let routes = vec![crate::domain::model::Route {
        enabled: true,
        match_config: http_match("/v1", &[HttpMethod::Get, HttpMethod::Post]),
        ..Default::default()
    }];
    assert!(best_matching_route(&routes, ProxyMethod::Get, "/v1").is_some());
    // The path did resolve, so the data plane reports the method as a guard
    // rejection (400) rather than the route as missing (404).
    let (route, _) = best_matching_route(&routes, ProxyMethod::Delete, "/v1").unwrap();
    assert_eq!(
        route.match_config.http.unwrap().methods,
        vec![HttpMethod::Get, HttpMethod::Post]
    );
}

#[test]
fn disabled_routes_are_skipped() {
    let routes = vec![
        crate::domain::model::Route {
            enabled: false,
            match_config: http_match("/v1/chat", &[HttpMethod::Get]),
            ..Default::default()
        },
        crate::domain::model::Route {
            enabled: true,
            match_config: http_match("/v1", &[HttpMethod::Get]),
            ..Default::default()
        },
    ];
    let (route, _) = best_matching_route(&routes, ProxyMethod::Get, "/v1/chat").unwrap();
    assert_eq!(route.match_config.http.unwrap().path, "/v1");
}

#[tokio::test]
async fn resolution_merges_upstream_and_route_configuration() {
    let svc = service();
    let mut upstream = host_upstream("api.openai.com", 80);
    upstream.headers.request.passthrough = crate::domain::model::PassthroughMode::All;
    upstream.rate_limit = Some(RateLimitConfig {
        sustained: SustainedRate {
            rate: 10,
            window: crate::domain::model::RateWindow::Second,
        },
        burst: Some(crate::domain::model::Burst { capacity: 20 }),
        ..Default::default()
    });
    upstream.plugins.items.push(PluginBinding::Reference(
        crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS.to_owned(),
    ));
    let upstream = svc.create_upstream(TENANT, upstream, None).await.unwrap();

    let mut route = route_for(upstream.id, "/v1", &[HttpMethod::Get]);
    route.rate_limit = Some(RateLimitConfig {
        sustained: SustainedRate {
            rate: 2,
            window: crate::domain::model::RateWindow::Second,
        },
        ..Default::default()
    });
    route.plugins.items.push(PluginBinding::Reference(
        crate::domain::gts_helpers::TRANSFORM_REQUEST_ID.to_owned(),
    ));
    let route = svc.create_route(TENANT, route).await.unwrap();

    let target = svc
        .resolve_proxy_target(TENANT, "api.openai.com", ProxyMethod::Get, "/v1/chat")
        .await
        .unwrap();
    assert_eq!(target.upstream.id, upstream.id);
    assert_eq!(target.route.id, route.id);
    assert_eq!(target.path_remainder, "/chat");
    assert_eq!(target.owning_tenant, TENANT);
    // Upstream plugins first, then route plugins.
    assert_eq!(
        target
            .plugins
            .iter()
            .map(PluginBinding::plugin_ref)
            .collect::<Vec<_>>(),
        vec![
            crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS,
            crate::domain::gts_helpers::TRANSFORM_REQUEST_ID
        ]
    );
    // The strictest rate limit wins.
    let limit = target.rate_limit.unwrap();
    assert_eq!(limit.sustained.rate, 2);
    assert_eq!(limit.capacity(), 2);
    assert!(target.cors.is_none());
}

#[tokio::test]
async fn resolution_rejects_unknown_alias_and_unmatched_paths() {
    let (svc, _upstream) = upstream_with_route("/v1", &[HttpMethod::Get]).await;
    assert!(matches!(
        svc.resolve_proxy_target(TENANT, "unknown.example.com", ProxyMethod::Get, "/")
            .await
            .unwrap_err(),
        DomainError::NotFound(_)
    ));
    assert!(matches!(
        svc.resolve_proxy_target(TENANT, "api.example.com", ProxyMethod::Get, "/other")
            .await
            .unwrap_err(),
        DomainError::RouteNotFound(_)
    ));
    // The path still resolves when no route admits the method; the data plane
    // turns that into a guard rejection (400), so resolution only reports the
    // route as missing when no prefix matches.
    let target = svc
        .resolve_proxy_target(TENANT, "api.example.com", ProxyMethod::Delete, "/v1")
        .await
        .unwrap();
    assert!(target.path_remainder.is_empty());
}

#[tokio::test]
async fn disabled_upstream_is_unavailable() {
    let svc = service();
    let mut upstream = host_upstream("api.openai.com", 80);
    upstream.enabled = false;
    let upstream = svc.create_upstream(TENANT, upstream, None).await.unwrap();
    svc.create_route(TENANT, route_for(upstream.id, "/", &[HttpMethod::Get]))
        .await
        .unwrap();
    assert!(matches!(
        svc.resolve_proxy_target(TENANT, "api.openai.com", ProxyMethod::Get, "/")
            .await
            .unwrap_err(),
        DomainError::LinkUnavailable(_)
    ));
}

#[tokio::test]
async fn path_suffix_disabled_rejects_a_suffix() {
    let svc = service();
    let upstream = svc
        .create_upstream(TENANT, host_upstream("api.example.com", 80), None)
        .await
        .unwrap();
    let mut route = route_for(upstream.id, "/v1", &[HttpMethod::Get]);
    route.match_config.http.as_mut().unwrap().path_suffix_mode =
        crate::domain::model::PathSuffixMode::Disabled;
    svc.create_route(TENANT, route).await.unwrap();

    if let Err(e) = svc
        .resolve_proxy_target(TENANT, "api.example.com", ProxyMethod::Get, "/v1")
        .await
    {
        panic!("exact-path request should resolve: {e:?}");
    }
    // The control plane still reports the match; enforcing `path_suffix_mode` is
    // the data plane's job, which is where the suffix is turned into an error.
    let target = svc
        .resolve_proxy_target(TENANT, "api.example.com", ProxyMethod::Get, "/v1/extra")
        .await
        .unwrap();
    assert_eq!(target.path_remainder, "/extra");
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_lifecycle_and_in_use_protection() {
    let (svc, upstream) = upstream_with_route("/v1", &[HttpMethod::Get]).await;

    let plugin = svc
        .create_plugin(
            TENANT,
            crate::domain::model::Plugin {
                name: "add-header".to_owned(),
                plugin_type: "transform".to_owned(),
                source_code: "def on_request(ctx): pass".to_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(svc.list_plugins(TENANT).await.unwrap().len(), 1);

    // Unknown plugin types are refused.
    let err = svc
        .create_plugin(
            TENANT,
            crate::domain::model::Plugin {
                name: "bad".to_owned(),
                plugin_type: "logger".to_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Validation(_)), "{err:?}");

    // Unreferenced plugins delete cleanly.
    svc.delete_plugin(TENANT, plugin.id).await.unwrap();

    // A plugin bound to a route is protected.
    let bound = svc
        .create_plugin(
            TENANT,
            crate::domain::model::Plugin {
                name: "bound".to_owned(),
                plugin_type: "transform".to_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let routes = svc.list_routes(TENANT).await.unwrap();
    let route_id = routes[0].id;
    let mut with_plugin = route_for(upstream.id, "/v1", &[HttpMethod::Get]);
    with_plugin.plugins.items.push(PluginBinding::Bound {
        plugin_ref: bound.id.to_string(),
        plugin_uuid: Some(bound.id),
        config: None,
    });
    svc.replace_route(TENANT, route_id, with_plugin)
        .await
        .unwrap();

    let DomainError::PluginInUse {
        upstreams,
        routes,
        upstream_ids,
        route_ids,
    } = svc.delete_plugin(TENANT, bound.id).await.unwrap_err()
    else {
        panic!("expected PluginInUse");
    };
    assert_eq!(upstreams, 0);
    assert_eq!(routes, 1);
    assert!(upstream_ids.is_empty());
    assert_eq!(route_ids.len(), 1, "the referencing route is named");
}

#[test]
fn plugin_binding_validation() {
    // Guard and transform identifiers bind through `plugins.items`.
    assert!(
        validate_plugin_binding(&PluginBinding::Reference(
            crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS.to_owned()
        ))
        .is_ok()
    );
    assert!(
        validate_plugin_binding(&PluginBinding::Bound {
            plugin_ref: crate::domain::gts_helpers::TRANSFORM_REQUEST_ID.to_owned(),
            plugin_uuid: None,
            config: None,
        })
        .is_ok()
    );
    // Catalog-only identifiers and unknown ones are refused.
    assert!(
        validate_plugin_binding(&PluginBinding::Reference(
            crate::domain::gts_helpers::TRANSFORM_METRICS.to_owned()
        ))
        .is_err()
    );
    assert!(validate_plugin_binding(&PluginBinding::Reference(String::new())).is_err());
}

#[test]
fn auth_plugin_ref_validation() {
    use crate::domain::gts_helpers as g;
    use crate::domain::services::management::validate_auth_plugin_ref;
    for id in [
        g::AUTH_NOOP,
        g::AUTH_APIKEY,
        g::AUTH_OAUTH2_CC,
        g::AUTH_OAUTH2_CC_BASIC,
    ] {
        assert!(validate_auth_plugin_ref(id).is_ok(), "{id}");
    }
    // Catalog-only identifiers must not be configured.
    assert!(validate_auth_plugin_ref(g::AUTH_BASIC).is_err());
    assert!(validate_auth_plugin_ref("gts.cf.core.oagw.unknown.v1~x").is_err());
}

#[test]
fn cors_validation_rules() {
    let mut cors = crate::domain::model::CorsConfig {
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        ..Default::default()
    };
    assert!(validate_cors(&cors).is_ok());

    cors.allow_credentials = true;
    cors.allowed_origins = vec!["*".to_owned()];
    assert!(validate_cors(&cors).is_err());

    cors.allowed_origins = vec!["not an origin".to_owned()];
    cors.allow_credentials = false;
    assert!(validate_cors(&cors).is_err());
}

#[test]
fn scheme_admission_follows_the_connection_policy() {
    assert!(check_scheme_admission(EndpointScheme::Http, true).is_ok());
    assert!(check_scheme_admission(EndpointScheme::Http, false).is_err());
    assert!(check_scheme_admission(EndpointScheme::Https, false).is_ok());
}

#[test]
fn proxy_method_parsing() {
    assert_eq!(ProxyMethod::parse("get"), ProxyMethod::Get);
    assert_eq!(ProxyMethod::parse("PATCH").as_str(), "PATCH");
    assert_eq!(ProxyMethod::parse("TRACE"), ProxyMethod::Other);
    assert_ne!(ProxyMethod::Get, ProxyMethod::Post);
}

// ---------------------------------------------------------------------------
// Hierarchical resolution
// ---------------------------------------------------------------------------

struct ParentFirst {
    chain: Vec<String>,
}

#[async_trait::async_trait]
impl TenantHierarchy for ParentFirst {
    async fn chain(&self, _tenant_id: &str) -> Vec<String> {
        self.chain.clone()
    }
}

#[tokio::test]
async fn ancestor_upstream_is_inherited_and_enforced_limits_apply() {
    let parent = "00000000-0000-0000-0000-000000000010";
    let child = "00000000-0000-0000-0000-000000000011";
    let upstreams = Arc::new(MemoryUpstreamRepository::default());
    let svc = ControlPlaneService::new(
        upstreams.clone(),
        Arc::new(MemoryRouteRepository::default()),
        Arc::new(MemoryPluginRepository::default()),
        Arc::new(ParentFirst {
            chain: vec![child.to_owned(), parent.to_owned()],
        }),
        true,
    );

    let mut parent_upstream = host_upstream("api.openai.com", 80);
    parent_upstream.rate_limit = Some(RateLimitConfig {
        sharing: crate::domain::model::SharingMode::Enforce,
        sustained: SustainedRate {
            rate: 3,
            window: crate::domain::model::RateWindow::Second,
        },
        ..Default::default()
    });
    let created = svc
        .create_upstream(parent, parent_upstream, None)
        .await
        .unwrap();

    // A route owned by the parent's upstream is visible to the child tenant.
    svc.create_route(parent, route_for(created.id, "/v1", &[HttpMethod::Get]))
        .await
        .unwrap();

    let target = svc
        .resolve_proxy_target(child, "api.openai.com", ProxyMethod::Get, "/v1/x")
        .await
        .unwrap();
    assert_eq!(target.owning_tenant, parent);
    assert_eq!(target.rate_limit.unwrap().sustained.rate, 3);
    assert_eq!(target.path_remainder, "/x");
}
