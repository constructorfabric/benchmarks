//! Tests of the control plane: tenant scoping, alias rules, match conflicts
//! and the proxy-time chain walk.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::ErrorKind;
use crate::domain::hierarchy::{StaticTenantHierarchy, TenantHierarchy};
use crate::domain::model::{
    AuthConfig, Endpoint, EndpointScheme, HttpMatch, MatchConfig, PathSuffixMode, PluginBinding,
    PluginsConfig, RouteSpec, UpstreamSpec,
};
use crate::domain::services::management::ControlPlaneService;
use crate::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};

fn context() -> SecurityContext {
    SecurityContext::anonymous()
}

fn endpoint(host: &str) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port: None,
    }
}

fn upstream_spec(alias: Option<&str>, host: &str) -> UpstreamSpec {
    UpstreamSpec {
        alias: alias.map(str::to_owned),
        server: crate::domain::model::ServerConfig {
            endpoints: vec![endpoint(host)],
        },
        ..UpstreamSpec::default()
    }
}

fn route_spec(method: &str, path: &str) -> RouteSpec {
    RouteSpec {
        match_rule: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![method.to_owned()],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        ..RouteSpec::default()
    }
}

fn service(hierarchy: Arc<dyn TenantHierarchy>) -> ControlPlaneService {
    ControlPlaneService::new(
        Arc::new(InMemoryUpstreamRepo::new()),
        Arc::new(InMemoryRouteRepo::new()),
        Arc::new(InMemoryPluginRepo::new()),
        hierarchy,
    )
}

#[tokio::test]
async fn creates_and_reads_an_upstream_of_its_tenant() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    let created = service
        .create_upstream(&context(), tenant, upstream_spec(None, "api.openai.com"))
        .await
        .expect("create");
    assert_eq!(created.alias(), "api.openai.com");
    let fetched = service.get_upstream(tenant, created.id).expect("get");
    assert_eq!(fetched.id, created.id);
    assert_eq!(service.list_upstreams(tenant).expect("list").len(), 1);
}

#[tokio::test]
async fn ip_based_endpoints_require_an_explicit_alias() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    let mut spec = upstream_spec(None, "10.0.0.1");
    spec.server.endpoints[0].scheme = crate::domain::model::EndpointScheme::Http;
    spec.server.endpoints[0].port = Some(8080);
    let error = service
        .create_upstream(&context(), tenant, spec)
        .await
        .expect_err("no alias");
    assert_eq!(error.kind, ErrorKind::ValidationError);
    let created = service
        .create_upstream(
            &context(),
            tenant,
            upstream_spec(Some("internal-svc"), "10.0.0.1"),
        )
        .await
        .expect("explicit alias");
    assert_eq!(created.alias(), "internal-svc");
}

#[tokio::test]
async fn upstream_reads_are_scoped_to_the_calling_tenant() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let created = service
        .create_upstream(&context(), owner, upstream_spec(None, "api.openai.com"))
        .await
        .expect("create");
    let error = service
        .get_upstream(other, created.id)
        .expect_err("cross tenant");
    assert_eq!(error.kind, ErrorKind::UpstreamNotFound);
    assert!(service.list_upstreams(other).expect("list").is_empty());
    let error = service
        .delete_upstream(other, created.id)
        .expect_err("cross tenant");
    assert_eq!(error.kind, ErrorKind::UpstreamNotFound);
}

#[tokio::test]
async fn alias_conflicts_inside_a_tenant_are_rejected() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    service
        .create_upstream(&context(), tenant, upstream_spec(None, "api.openai.com"))
        .await
        .expect("create");
    let error = service
        .create_upstream(
            &context(),
            tenant,
            upstream_spec(Some("api.openai.com"), "10.0.0.9"),
        )
        .await
        .expect_err("same alias");
    assert_eq!(error.kind, ErrorKind::Conflict);
}

#[tokio::test]
async fn an_ancestor_alias_may_be_shadowed_by_a_descendant() {
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let mut hierarchy = StaticTenantHierarchy::single(root);
    hierarchy.add(child, root);
    let service = service(Arc::new(hierarchy));
    let ancestor = service
        .create_upstream(&context(), root, upstream_spec(None, "vendor.com"))
        .await
        .expect("ancestor upstream");
    let descendant = service
        .create_upstream(
            &context(),
            child,
            upstream_spec(Some("vendor.com"), "10.0.0.5"),
        )
        .await
        .expect("a descendant may bind an ancestor alias");
    assert_ne!(descendant.id, ancestor.id);

    // An ancestor that enforces its configuration blocks the override.
    let mut enforced_spec = upstream_spec(Some("enforced.example.com"), "10.0.0.8");
    enforced_spec.rate_limit = Some(crate::domain::model::RateLimitConfig {
        sharing: crate::domain::model::SharingMode::Enforce,
        algorithm: crate::domain::model::RateLimitAlgorithm::TokenBucket,
        sustained: crate::domain::model::SustainedRate {
            rate: 10,
            window: crate::domain::model::RateWindow::Second,
        },
        burst: None,
        scope: crate::domain::model::RateScope::Tenant,
        strategy: crate::domain::model::RateStrategy::Reject,
        response_headers: true,
        cost: 1,
    });
    service
        .create_upstream(&context(), root, enforced_spec)
        .await
        .expect("enforcing ancestor");
    let error = service
        .create_upstream(
            &context(),
            child,
            upstream_spec(Some("enforced.example.com"), "10.0.0.9"),
        )
        .await
        .expect_err("enforced alias");
    assert_eq!(error.kind, ErrorKind::Conflict);
}

#[tokio::test]
async fn the_chain_walk_prefers_the_closest_upstream() {
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let grandchild = Uuid::new_v4();
    let mut hierarchy = StaticTenantHierarchy::single(root);
    hierarchy.add(child, root);
    hierarchy.add(grandchild, child);
    let service = service(Arc::new(hierarchy));

    let root_upstream = service
        .create_upstream(&context(), root, upstream_spec(None, "vendor.com"))
        .await
        .expect("root upstream");
    let child_upstream = service
        .create_upstream(
            &context(),
            child,
            upstream_spec(Some("vendor.com"), "10.0.0.5"),
        )
        .await
        .expect("child upstream");
    assert_eq!(child_upstream.alias(), "vendor.com");

    let resolved = service
        .resolve_alias(&context(), grandchild, "vendor.com")
        .await
        .expect("child");
    assert_eq!(resolved.id, child_upstream.id);
    let resolved = service
        .resolve_alias(&context(), child, "vendor.com")
        .await
        .expect("self");
    assert_eq!(resolved.id, child_upstream.id);
    let resolved = service
        .resolve_alias(&context(), root, "vendor.com")
        .await
        .expect("root");
    assert_eq!(resolved.id, root_upstream.id);
    let expected_id = format!("gts.cf.core.oagw.upstream.v1~{}", root_upstream.id);
    assert_eq!(root_upstream.gts_id(), expected_id);
}

#[tokio::test]
async fn disabled_upstreams_are_skipped_by_the_chain_walk() {
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let mut hierarchy = StaticTenantHierarchy::single(root);
    hierarchy.add(child, root);
    let service = service(Arc::new(hierarchy));
    service
        .create_upstream(&context(), root, upstream_spec(None, "vendor.com"))
        .await
        .expect("root upstream");

    let mut disabled = upstream_spec(Some("vendor.com"), "10.0.0.5");
    disabled.enabled = false;
    service
        .create_upstream(&context(), child, disabled)
        .await
        .expect("child upstream");
    let resolved = service
        .resolve_alias(&context(), child, "vendor.com")
        .await
        .expect("falls back");
    assert_eq!(resolved.tenant_id, root);
}

#[tokio::test]
async fn unknown_aliases_are_not_found() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let error = service
        .resolve_alias(&context(), Uuid::new_v4(), "vendor.com")
        .await;
    assert!(matches!(error, Err(ref e) if e.kind == ErrorKind::UpstreamNotFound));
}

#[tokio::test]
async fn routes_require_an_upstream_of_the_same_tenant() {
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let mut hierarchy = StaticTenantHierarchy::single(root);
    hierarchy.add(child, root);
    let service = service(Arc::new(hierarchy));
    let upstream = service
        .create_upstream(&context(), root, upstream_spec(None, "vendor.com"))
        .await
        .expect("root upstream");
    let error = service
        .create_route(child, upstream.id, route_spec("GET", "/v1"))
        .expect_err("ancestor upstream is not addressable");
    assert_eq!(error.kind, ErrorKind::UpstreamNotFound);

    let own = service
        .create_upstream(&context(), child, upstream_spec(None, "child.example.com"))
        .await
        .expect("child upstream");
    let route = service
        .create_route(child, own.id, route_spec("GET", "/v1"))
        .expect("own upstream");
    assert_eq!(route.upstream_id, own.id);
    assert_eq!(
        service
            .list_routes(child, Some(own.id))
            .expect("list")
            .len(),
        1
    );
}

#[tokio::test]
async fn duplicate_match_rules_are_rejected() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(&context(), tenant, upstream_spec(None, "vendor.com"))
        .await
        .expect("upstream");
    service
        .create_route(tenant, upstream.id, route_spec("GET", "/v1"))
        .expect("first route");
    let error = service
        .create_route(tenant, upstream.id, route_spec("GET", "/v1"))
        .expect_err("same path and method");
    assert_eq!(error.kind, ErrorKind::Conflict);

    let other_upstream = service
        .create_upstream(&context(), tenant, upstream_spec(None, "other.example.com"))
        .await
        .expect("other upstream");
    service
        .create_route(tenant, other_upstream.id, route_spec("GET", "/v1"))
        .expect("a different upstream may reuse the rule");
}

#[tokio::test]
async fn route_replacement_keeps_the_upstream_reference() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(&context(), tenant, upstream_spec(None, "vendor.com"))
        .await
        .expect("upstream");
    let route = service
        .create_route(tenant, upstream.id, route_spec("GET", "/v1"))
        .expect("route");
    let updated = service
        .replace_route(tenant, route.id, route_spec("POST", "/v1"))
        .expect("replace");
    assert_eq!(updated.upstream_id, upstream.id);
    assert_eq!(
        service
            .get_route(tenant, route.id)
            .expect("get")
            .spec
            .match_rule
            .http
            .expect("http")
            .methods,
        vec!["POST".to_owned()]
    );
}

#[tokio::test]
async fn deleting_an_upstream_removes_its_routes() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(&context(), tenant, upstream_spec(None, "vendor.com"))
        .await
        .expect("upstream");
    let route = service
        .create_route(tenant, upstream.id, route_spec("GET", "/v1"))
        .expect("route");
    service
        .delete_upstream(tenant, upstream.id)
        .expect("delete");
    assert!(service.get_upstream(tenant, upstream.id).is_err());
    assert!(service.get_route(tenant, route.id).is_err());
}

#[tokio::test]
async fn plugins_are_immutable_and_in_use_checks_apply() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    let plugin = service
        .create_plugin(
            tenant,
            crate::domain::model::PluginSpec {
                plugin_type: "guard_plugin".to_owned(),
                name: "tenant-guard".to_owned(),
                source_code: Some("def on_request(ctx): pass".to_owned()),
                config_schema: None,
            },
        )
        .expect("plugin");
    let reference = format!("gts.cf.core.oagw.guard_plugin.v1~{}", plugin.id);

    let mut spec = upstream_spec(None, "vendor.com");
    spec.plugins = Some(PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![PluginBinding::Reference(reference.clone())],
        config: None,
    });
    let upstream = service
        .create_upstream(&context(), tenant, spec)
        .await
        .expect("upstream");
    let error = service
        .delete_plugin(tenant, plugin.id)
        .expect_err("in use");
    assert_eq!(error.kind, ErrorKind::PluginInUse);

    service
        .delete_upstream(tenant, upstream.id)
        .expect("delete upstream");
    service
        .delete_plugin(tenant, plugin.id)
        .expect("delete after unbind");
}

#[tokio::test]
async fn unknown_bound_plugins_are_rejected() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    let mut spec = upstream_spec(None, "vendor.com");
    spec.plugins = Some(PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![PluginBinding::Reference(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1".to_owned(),
        )],
        config: None,
    });
    let error = service
        .create_upstream(&context(), tenant, spec)
        .await
        .expect_err("catalog-only");
    assert_eq!(error.kind, ErrorKind::PluginNotFound);

    let mut auth = upstream_spec(None, "vendor.com");
    auth.auth = Some(AuthConfig {
        auth_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1".to_owned(),
        sharing: crate::domain::model::SharingMode::Private,
        config: None,
    });
    let error = service
        .create_upstream(&context(), tenant, auth)
        .await
        .expect_err("basic auth");
    assert_eq!(error.kind, ErrorKind::PluginNotFound);
}

#[tokio::test]
async fn the_proxy_walk_prefers_descendant_routes() {
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let mut hierarchy = StaticTenantHierarchy::single(root);
    hierarchy.add(child, root);
    let service = service(Arc::new(hierarchy));

    let upstream = service
        .create_upstream(&context(), root, upstream_spec(None, "vendor.com"))
        .await
        .expect("root upstream");
    let root_route = service
        .create_route(root, upstream.id, route_spec("GET", "/v1"))
        .expect("root route");

    let target = service
        .resolve_proxy_target(&context(), child, "vendor.com", "GET", "/v1/models")
        .await
        .expect("resolved");
    assert_eq!(target.upstream.id, upstream.id);
    assert_eq!(target.route.id, root_route.id);
    assert_eq!(target.upstream_path, "/v1/models");
    assert_eq!(target.upstream.alias(), "vendor.com");
}

#[tokio::test]
async fn descendant_routes_override_ancestor_routes() {
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let mut hierarchy = StaticTenantHierarchy::single(root);
    hierarchy.add(child, root);
    let service = service(Arc::new(hierarchy));

    let root_upstream = service
        .create_upstream(&context(), root, upstream_spec(None, "vendor.com"))
        .await
        .expect("root upstream");
    service
        .create_route(root, root_upstream.id, route_spec("GET", "/v1"))
        .expect("root route");

    let child_upstream = service
        .create_upstream(
            &context(),
            child,
            upstream_spec(Some("vendor.com"), "10.0.0.5"),
        )
        .await
        .expect("child upstream");
    let child_route = service
        .create_route(child, child_upstream.id, route_spec("GET", "/v1"))
        .expect("child route");

    let target = service
        .resolve_proxy_target(&context(), child, "vendor.com", "GET", "/v1/models")
        .await
        .expect("resolved");
    assert_eq!(target.route.id, child_route.id);
}

#[tokio::test]
async fn missing_routes_and_methods_are_reported() {
    let service = service(Arc::new(StaticTenantHierarchy::default()));
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(&context(), tenant, upstream_spec(None, "vendor.com"))
        .await
        .expect("upstream");
    service
        .create_route(tenant, upstream.id, route_spec("GET", "/v1"))
        .expect("route");
    let error = service
        .resolve_proxy_target(&context(), tenant, "vendor.com", "POST", "/v1/models")
        .await
        .expect_err("method not allowed");
    assert_eq!(error.kind, ErrorKind::RouteNotFound);
    let error = service
        .resolve_proxy_target(&context(), tenant, "vendor.com", "GET", "/v2/other")
        .await
        .expect_err("path not matched");
    assert_eq!(error.kind, ErrorKind::RouteNotFound);
}
