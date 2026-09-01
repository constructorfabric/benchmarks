//! Tests for [`crate::domain::services`].

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{ControlPlaneService, NoHierarchy, TenantHierarchy};
use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::model::{CorsConfig, Endpoint, Protocol, Scheme, ServerConfig, SharingMode};
use crate::domain::validation::{RouteInput, UpstreamInput, Validator, route_match_key};
use crate::infra::storage::{CacheLimits, RegistryStore};

const TENANT: Uuid = Uuid::from_u128(0x11);
const ANCESTOR: Uuid = Uuid::from_u128(0x22);

fn context(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x33))
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

/// Hierarchy with a single fixed ancestor, so the bind rule is testable
/// without a tenant resolver.
#[derive(Debug, Default)]
struct FixedHierarchy {
    ancestor: std::sync::Mutex<Option<Uuid>>,
}

#[async_trait]
impl TenantHierarchy for FixedHierarchy {
    async fn ancestors(&self, _ctx: &SecurityContext, _tenant: Uuid) -> Vec<Uuid> {
        self.ancestor
            .lock()
            .map(|guard| guard.iter().copied().collect())
            .unwrap_or_default()
    }
}

fn endpoint(host: &str) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: host.to_owned(),
        port: 443,
    }
}

fn upstream_input(hosts: &[&str], alias: Option<&str>) -> UpstreamInput {
    UpstreamInput {
        alias: alias.map(ToOwned::to_owned),
        enabled: None,
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: hosts.iter().map(|host| endpoint(host)).collect(),
        },
        protocol: Protocol::Http,
        auth: None,
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
    }
}

fn limits() -> CacheLimits {
    CacheLimits {
        upstream: 16,
        route: 16,
        plugin: 16,
        dp: 16,
    }
}

fn service() -> ControlPlaneService {
    service_with_hierarchy(NoHierarchy)
}

fn service_with_hierarchy(hierarchy: impl TenantHierarchy + 'static) -> ControlPlaneService {
    ControlPlaneService::new(
        Arc::new(RegistryStore::new(limits())),
        Validator::new(OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        }),
        Arc::new(hierarchy),
    )
}

async fn create_upstream(
    svc: &ControlPlaneService,
    tenant: Uuid,
    hosts: &[&str],
    alias: Option<&str>,
) -> Result<Arc<crate::domain::model::Upstream>, OagwError> {
    let input = upstream_input(hosts, alias);
    svc.create_upstream(&context(tenant), Some("req-1"), &input)
        .await
}

#[tokio::test]
async fn an_upstream_is_created_for_the_calling_tenant() {
    let svc = service();
    let created = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    assert_eq!(created.tenant_id, TENANT);
    assert_eq!(created.alias, "api.openai.com");
    assert!(created.enabled, "enabled defaults to true");
    assert_ne!(created.id, Uuid::nil(), "the store assigns an id");

    // The alias is resolvable for the owning tenant only.
    assert!(
        svc.store()
            .resolve_upstream_alias(&[TENANT], "api.openai.com")
            .is_some()
    );
    assert!(
        svc.store()
            .resolve_upstream_alias(&[ANCESTOR], "api.openai.com")
            .is_none()
    );
}

#[tokio::test]
async fn a_duplicate_alias_conflicts() {
    let svc = service();
    create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    let error = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect_err("duplicate alias");
    assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_same_alias_in_another_tenant_is_fine() {
    let svc = service();
    create_upstream(&svc, ANCESTOR, &["api.openai.com"], None)
        .await
        .expect("created");
    create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("a sibling tenant may reuse the alias");
}

#[tokio::test]
async fn an_enforced_ancestor_alias_is_forbidden() {
    let hierarchy = FixedHierarchy::default();
    *hierarchy.ancestor.lock().expect("lock") = Some(ANCESTOR);
    let svc = service_with_hierarchy(hierarchy);

    let owner = create_upstream(&svc, ANCESTOR, &["api.openai.com"], None)
        .await
        .expect("ancestor upstream");
    svc.store()
        .get_upstream(ANCESTOR, owner.id)
        .expect("stored");

    // Flip the ancestor to `enforce`: the descendant is rejected with 403.
    let enforced = crate::domain::model::Upstream {
        plugins: crate::domain::model::PluginConfig {
            sharing: SharingMode::Enforce,
            items: Vec::new(),
        },
        ..(*svc
            .store()
            .get_upstream(ANCESTOR, owner.id)
            .expect("stored"))
        .clone()
    };
    svc.store().replace_upstream(enforced).expect("replaced");

    let error = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect_err("ancestor enforces the alias");
    assert_eq!(error.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(error.context().alias.as_deref(), Some("api.openai.com"));
}

#[tokio::test]
async fn a_private_ancestor_alias_is_bindable() {
    let hierarchy = FixedHierarchy::default();
    *hierarchy.ancestor.lock().expect("lock") = Some(ANCESTOR);
    let svc = service_with_hierarchy(hierarchy);
    create_upstream(&svc, ANCESTOR, &["api.openai.com"], None)
        .await
        .expect("ancestor upstream");
    create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("private ancestor configuration does not block the bind");
}

#[tokio::test]
async fn replace_keeps_the_alias_and_the_tenant() {
    let svc = service();
    let created = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    let replacement = upstream_input(&["eu.api.openai.com", "us.api.openai.com"], None);
    let replaced = svc
        .replace_upstream(&context(TENANT), Some("req-2"), created.id, &replacement)
        .await
        .expect("replaced");
    assert_eq!(replaced.id, created.id);
    assert_eq!(replaced.alias, "api.openai.com");
    assert_eq!(replaced.tenant_id, TENANT);
    assert_eq!(replaced.server.endpoints.len(), 2);
}

#[tokio::test]
async fn replacing_a_foreign_upstream_is_a_404() {
    let svc = service();
    let created = create_upstream(&svc, ANCESTOR, &["api.openai.com"], None)
        .await
        .expect("created");
    let error = svc
        .replace_upstream(
            &context(TENANT),
            None,
            created.id,
            &upstream_input(&["api.openai.com"], None),
        )
        .await
        .expect_err("not owned by the caller");
    assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_upstream_cascades_its_routes() {
    let svc = service();
    let created = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    let route = svc
        .create_route(
            &context(TENANT),
            None,
            created.id,
            &route_input("/v1", &["GET"]),
        )
        .await
        .expect("route");
    let removed = svc
        .delete_upstream(&context(TENANT), Some("req-3"), created.id)
        .expect("deleted");
    assert_eq!(removed.id, created.id);
    assert!(
        svc.get_route(&context(TENANT), route.id).is_none(),
        "routes cascade"
    );
    let error = svc
        .delete_upstream(&context(TENANT), None, created.id)
        .expect_err("already gone");
    assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
}

// -- routes -------------------------------------------------------------------

fn route_input(path: &str, methods: &[&str]) -> RouteInput {
    RouteInput {
        r#match: crate::domain::model::RouteMatch {
            http: Some(crate::domain::model::HttpMatch {
                methods: methods
                    .iter()
                    .map(|method| match *method {
                        "GET" => crate::domain::model::HttpMethod::Get,
                        "POST" => crate::domain::model::HttpMethod::Post,
                        _ => crate::domain::model::HttpMethod::Delete,
                    })
                    .collect(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
        enabled: None,
        priority: None,
        tags: Vec::new(),
    }
}

#[tokio::test]
async fn a_route_is_created_under_an_owned_upstream() {
    let svc = service();
    let owner = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    let route = svc
        .create_route(
            &context(TENANT),
            Some("req-4"),
            owner.id,
            &route_input("/v1", &["GET"]),
        )
        .await
        .expect("route");
    assert_eq!(route.upstream_id, owner.id);
    assert_eq!(route.tenant_id, TENANT);
    assert_eq!(route.priority, 0, "priority defaults to 0");
    assert_eq!(
        route_match_key(&route),
        crate::domain::validation::MatchKey::Http {
            path: "/v1".to_owned(),
            methods: [crate::domain::model::HttpMethod::Get]
                .into_iter()
                .collect(),
        }
    );
}

#[tokio::test]
async fn a_route_for_a_foreign_upstream_is_a_404() {
    let svc = service();
    let owner = create_upstream(&svc, ANCESTOR, &["api.openai.com"], None)
        .await
        .expect("created");
    let error = svc
        .create_route(
            &context(TENANT),
            None,
            owner.id,
            &route_input("/v1", &["GET"]),
        )
        .await
        .expect_err("upstream belongs to another tenant");
    assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_duplicate_match_rule_conflicts() {
    let svc = service();
    let owner = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    svc.create_route(
        &context(TENANT),
        None,
        owner.id,
        &route_input("/v1", &["GET"]),
    )
    .await
    .expect("first route");
    let error = svc
        .create_route(
            &context(TENANT),
            None,
            owner.id,
            &route_input("/v1", &["GET"]),
        )
        .await
        .expect_err("duplicate match rule");
    assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn replace_route_keeps_its_upstream() {
    let svc = service();
    let owner = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    let route = svc
        .create_route(
            &context(TENANT),
            None,
            owner.id,
            &route_input("/v1", &["GET"]),
        )
        .await
        .expect("route");
    let replacement = RouteInput {
        priority: Some(7),
        ..route_input("/v2", &["POST"])
    };
    let replaced = svc
        .replace_route(&context(TENANT), Some("req-5"), route.id, &replacement)
        .await
        .expect("replaced");
    assert_eq!(replaced.id, route.id);
    assert_eq!(replaced.upstream_id, owner.id, "upstream_id is immutable");
    assert_eq!(replaced.priority, 7);
}

#[tokio::test]
async fn routes_list_their_tenant_only() {
    let svc = service();
    let owner = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    let foreign = create_upstream(&svc, ANCESTOR, &["api.openai.com"], None)
        .await
        .expect("created");
    svc.create_route(
        &context(TENANT),
        None,
        owner.id,
        &route_input("/own", &["GET"]),
    )
    .await
    .expect("route");
    svc.create_route(
        &context(ANCESTOR),
        None,
        foreign.id,
        &route_input("/other", &["GET"]),
    )
    .await
    .expect("route");
    let listed = svc.list_routes(&context(TENANT));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].tenant_id, TENANT);
}

// -- plugins ------------------------------------------------------------------

fn plugin_input(plugin_type: &str) -> crate::domain::validation::PluginInput {
    crate::domain::validation::PluginInput {
        plugin_type: plugin_type.to_owned(),
        config: serde_json::json!({"headers": ["x-request-id"]}),
        enabled: None,
        tags: Vec::new(),
    }
}

const GUARD_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1";

#[tokio::test]
async fn a_plugin_is_created_and_rendered() {
    let svc = service();
    let plugin = svc
        .create_plugin(&context(TENANT), Some("req-6"), &plugin_input(GUARD_PLUGIN))
        .expect("created");
    assert_eq!(plugin.tenant_id, TENANT);
    assert!(plugin.enabled);

    let (_, source) = svc
        .plugin_source(&context(TENANT), plugin.id)
        .expect("source");
    assert!(
        source.contains("PLUGIN_TYPE = \"gts.cf.core.oagw.guard_plugin.v1\""),
        "{source}"
    );
}

#[tokio::test]
async fn a_plugin_in_use_cannot_be_deleted() {
    let svc = service();
    let plugin = svc
        .create_plugin(&context(TENANT), None, &plugin_input(GUARD_PLUGIN))
        .expect("created");
    let owner = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");

    // Bind the plugin into the upstream chain.
    let mut bound = (*owner).clone();
    bound
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            plugin.id.to_string(),
            serde_json::json!({}),
        ));
    svc.store().replace_upstream(bound).expect("rebound");

    let error = svc
        .delete_plugin(&context(TENANT), Some("req-7"), plugin.id)
        .expect_err("still referenced");
    assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    let expected_plugin = format!("gts.cf.core.oagw.plugin.v1~{}", plugin.id);
    assert_eq!(
        error.context().plugin_id.as_deref(),
        Some(expected_plugin.as_str())
    );
    let referenced = error
        .context()
        .referenced_by
        .as_ref()
        .expect("referenced_by");
    let expected_owner = format!("gts.cf.core.oagw.upstream.v1~{}", owner.id);
    assert_eq!(referenced.upstreams, vec![expected_owner]);
    assert!(referenced.routes.is_empty());

    // Unbinding clears the conflict.
    let unbound = (*owner).clone();
    svc.store().replace_upstream(unbound).expect("rebound");
    let removed = svc
        .delete_plugin(&context(TENANT), None, plugin.id)
        .expect("deleted");
    assert_eq!(removed.id, plugin.id);
}

#[tokio::test]
async fn a_plugin_in_use_by_a_route_cannot_be_deleted() {
    let svc = service();
    let plugin = svc
        .create_plugin(&context(TENANT), None, &plugin_input(GUARD_PLUGIN))
        .expect("created");
    let owner = create_upstream(&svc, TENANT, &["api.openai.com"], None)
        .await
        .expect("created");
    let mut route_input = route_input("/v1", &["GET"]);
    route_input
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            plugin.id.to_string(),
            serde_json::json!({}),
        ));
    let route = svc
        .create_route(&context(TENANT), None, owner.id, &route_input)
        .await
        .expect("route");

    let error = svc
        .delete_plugin(&context(TENANT), None, plugin.id)
        .expect_err("still referenced by the route");
    let referenced = error
        .context()
        .referenced_by
        .as_ref()
        .expect("referenced_by");
    let expected_route = format!("gts.cf.core.oagw.route.v1~{}", route.id);
    assert_eq!(referenced.routes, vec![expected_route]);
}

#[tokio::test]
async fn a_foreign_binding_blocks_the_delete_without_naming_foreign_resources() {
    let svc = service();
    let plugin = svc
        .create_plugin(&context(TENANT), None, &plugin_input(GUARD_PLUGIN))
        .expect("created");

    // A binding held by *another* tenant (a shared plugin resolved by a
    // sibling subsystem, a racing write, a future surface): the store holds an
    // upstream of `ANCESTOR` that references the caller's plugin.
    let mut bound = (*create_upstream(&svc, ANCESTOR, &["foreign.example.com"], None)
        .await
        .expect("created"))
    .clone();
    bound
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            plugin.id.to_string(),
            serde_json::json!({}),
        ));
    svc.store().replace_upstream(bound).expect("bound");

    let error = svc
        .delete_plugin(&context(TENANT), Some("req-8"), plugin.id)
        .expect_err("a foreign binding still blocks the delete");
    assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    // The count in the detail is tenant-wide, the body is not: the wire must
    // not name another tenant's resource ids (DESIGN section 3.3).
    assert!(error.detail().contains("1 upstream(s)"), "{error}");
    let referenced = error
        .context()
        .referenced_by
        .as_ref()
        .expect("referenced_by");
    assert!(referenced.upstreams.is_empty(), "{referenced:?}");
    assert!(referenced.routes.is_empty(), "{referenced:?}");

    // Unbinding clears the conflict.
    let mut unbound = (*svc
        .store()
        .resolve_upstream_alias(&[ANCESTOR], "foreign.example.com")
        .expect("stored"))
    .clone();
    unbound.plugins.items.clear();
    svc.store().replace_upstream(unbound).expect("unbound");
    let removed = svc
        .delete_plugin(&context(TENANT), None, plugin.id)
        .expect("deleted");
    assert_eq!(removed.id, plugin.id);
}

#[tokio::test]
async fn plugin_lists_are_tenant_scoped() {
    let svc = service();
    svc.create_plugin(&context(TENANT), None, &plugin_input(GUARD_PLUGIN))
        .expect("created");
    assert_eq!(svc.list_plugins(&context(TENANT)).len(), 1);
    assert!(svc.list_plugins(&context(ANCESTOR)).is_empty());
}

#[tokio::test]
async fn cors_conflicts_are_surfaced_as_validation_errors() {
    let svc = service();
    let mut input = upstream_input(&["api.openai.com"], None);
    input.cors = Some(CorsConfig {
        enabled: true,
        sharing: SharingMode::Private,
        allowed_origins: vec!["*".to_owned()],
        allowed_methods: Vec::new(),
        allow_headers: Vec::new(),
        expose_headers: Vec::new(),
        allow_credentials: true,
        max_age: None,
    });
    let error = svc
        .create_upstream(&context(TENANT), None, &input)
        .await
        .expect_err("credentials with a wildcard origin");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}
