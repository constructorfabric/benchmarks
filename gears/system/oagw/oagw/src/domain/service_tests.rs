//! Tests for the control-plane service (DESIGN §3.3 "CRUD Semantics" and
//! "Tenant Scoping").
//!
//! Review evidence (privilege boundary — tenant scoping):
//! * Guardrail: DESIGN §3.3 "Tenant Scoping" — every operation is scoped to the
//!   caller's tenant; ancestors are invisible and not addressable.
//! * Rationale: the service is the only place where the tenant check can be
//!   enforced for every transport, so the tests drive it directly through the
//!   store instead of through the REST layer.
//! * Validation performed: `tenant_scoping_*` asserts that a second tenant
//!   cannot read, list, delete or address a resource owned by the first, and
//!   that an upstream of another tenant cannot be used as a route target.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use uuid::Uuid;

use super::{
    ControlPlaneService, PluginDraft, RouteDraft, RouteUpdate, UpstreamDraft,
};
use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::domain::models::{
    Endpoint, EndpointScheme, HttpMatch, HttpMethod, MatchConfig, PluginKind, Protocol,
    ServerConfig,
};
use crate::infra::storage::InMemoryStore;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn service() -> ControlPlaneService {
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    ControlPlaneService::new(InMemoryStore::new(), &config)
}

fn server(hosts: &[&str]) -> ServerConfig {
    ServerConfig {
        endpoints: hosts
            .iter()
            .map(|host| Endpoint::new(EndpointScheme::Https, *host, 443))
            .collect(),
    }
}

fn http_match(method: HttpMethod, path: &str) -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods: vec![method],
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: crate::domain::models::PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

fn upstream_draft(hosts: &[&str]) -> UpstreamDraft {
    UpstreamDraft {
        alias: None,
        enabled: true,
        protocol: Protocol::Http,
        server: server(hosts),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    }
}

fn route_draft(upstream_id: Uuid, path: &str) -> RouteDraft {
    RouteDraft {
        upstream_id,
        enabled: true,
        priority: 0,
        match_config: http_match(HttpMethod::Get, path),
        plugins: None,
        rate_limit: None,
        tags: Vec::new(),
    }
}

fn plugin_draft(name: &str) -> PluginDraft {
    PluginDraft {
        plugin_type: PluginKind::Guard,
        name: name.to_owned(),
        description: None,
        phases: Vec::new(),
        config_schema: None,
        source_code: "def guard_request(ctx):\n    return {}\n".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// upstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_upstream_derives_alias_from_hostname() {
    let service = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();
    assert_eq!(upstream.alias, "api.openai.com");
    assert_eq!(upstream.tenant_id, tenant);
    assert!(upstream.enabled);
    assert_eq!(upstream.created_at, upstream.updated_at);
    assert!(upstream.gts_id().starts_with("gts.cf.core.oagw.upstream.v1~"));
}

#[tokio::test]
async fn create_upstream_collapses_pool_to_common_suffix() {
    let service = service();
    let upstream = service
        .create_upstream(
            Uuid::new_v4(),
            upstream_draft(&["us.vendor.com", "eu.vendor.com"]),
        )
        .await
        .unwrap();
    assert_eq!(upstream.alias, "vendor.com");
}

#[tokio::test]
async fn create_upstream_normalizes_the_supplied_alias() {
    let service = service();
    let mut draft = upstream_draft(&["api.openai.com"]);
    draft.alias = Some("Api.OpenAI.COM.".to_owned());
    let upstream = service.create_upstream(Uuid::new_v4(), draft).await.unwrap();
    assert_eq!(upstream.alias, "api.openai.com");
}

#[tokio::test]
async fn create_upstream_requires_explicit_alias_for_ip_pool() {
    let service = service();
    let err = service
        .create_upstream(Uuid::new_v4(), upstream_draft(&["198.51.100.7"]))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::AliasNotDerivable { .. }), "{err:?}");
    assert_eq!(err.status(), 400);
    assert!(err.detail().contains("explicit alias"), "{err:?}");
}

#[tokio::test]
async fn create_upstream_rejects_alias_that_differs_from_derivation() {
    let service = service();
    let mut draft = upstream_draft(&["api.openai.com"]);
    draft.alias = Some("example.org".to_owned());
    let err = service.create_upstream(Uuid::new_v4(), draft).await.unwrap_err();
    assert!(matches!(err, DomainError::AliasMismatch { .. }), "{err:?}");
    let extensions = err.extensions();
    assert_eq!(extensions.invalid_value.as_deref(), Some("example.org"));
    assert_eq!(extensions.alias.as_deref(), Some("api.openai.com"));
}

#[tokio::test]
async fn create_upstream_rejects_alias_conflict_within_a_tenant() {
    let service = service();
    let tenant = Uuid::new_v4();
    let first = service
        .create_upstream(tenant, upstream_draft(&["us.vendor.com", "eu.vendor.com"]))
        .await
        .unwrap();
    assert_eq!(first.alias, "vendor.com");

    // A second pool under the same registrable domain derives the same alias,
    // so the insert must collide on `(tenant_id, alias)`.
    let err = service
        .create_upstream(tenant, upstream_draft(&["us2.vendor.com", "eu2.vendor.com"]))
        .await
        .unwrap_err();
    match err {
        DomainError::AliasConflict {
            alias,
            existing_upstream_id,
        } => {
            assert_eq!(alias, "vendor.com");
            assert_eq!(existing_upstream_id, first.id);
        }
        other => panic!("expected AliasConflict, got {other:?}"),
    }
}

#[tokio::test]
async fn alias_conflict_is_not_shared_across_tenants() {
    let service = service();
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();
    service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();
    let second = service
        .create_upstream(other, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();
    assert_eq!(second.alias, "api.openai.com");
}

#[tokio::test]
async fn replace_upstream_keeps_a_stable_alias_and_bumps_updated_at() {
    let service = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();

    let mut draft = upstream_draft(&["api.openai.com"]);
    draft.tags = vec!["rotated".to_owned()];
    let replaced = service
        .replace_upstream(tenant, upstream.id, draft)
        .await
        .unwrap();
    assert_eq!(replaced.alias, "api.openai.com");
    assert_eq!(replaced.tags, vec!["rotated".to_owned()]);
    assert_eq!(replaced.id, upstream.id);
}

#[tokio::test]
async fn replace_upstream_rejects_an_alias_change() {
    let service = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();

    // DESIGN §3.1 "Alias Update Behavior": derivable → derivable with a
    // different derived alias is rejected.
    let err = service
        .replace_upstream(tenant, upstream.id, upstream_draft(&["api.anthropic.com"]))
        .await
        .unwrap_err();
    assert_eq!(err.status(), 400);
    assert_eq!(err.extensions().invalid_value.as_deref(), Some("api.openai.com"));
}

#[tokio::test]
async fn delete_upstream_cascades_its_routes() {
    let service = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();
    let route = service
        .create_route(tenant, route_draft(upstream.id, "/v1/chat"))
        .await
        .unwrap();

    service.delete_upstream(tenant, upstream.id).await.unwrap();
    assert!(service.list_upstreams(tenant).await.unwrap().is_empty());
    assert!(service.list_routes(tenant).await.unwrap().is_empty());
    let err = service.get_route(tenant, route.id).await.unwrap_err();
    assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
}

#[tokio::test]
async fn lists_are_scoped_to_the_calling_tenant() {
    let service = service();
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();

    assert!(service.list_upstreams(other).await.unwrap().is_empty());
    let err = service.get_upstream(other, upstream.id).await.unwrap_err();
    assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
    assert!(matches!(
        service.delete_upstream(other, upstream.id).await.unwrap_err(),
        DomainError::NotFound { .. }
    ));
}

// ---------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_crud_round_trip_keeps_upstream_id_immutable() {
    let service = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();
    let route = service
        .create_route(tenant, route_draft(upstream.id, "/v1/chat"))
        .await
        .unwrap();
    assert_eq!(route.upstream_id, upstream.id);
    assert_eq!(route.priority, 0);

    let update = RouteUpdate {
        enabled: false,
        priority: 5,
        match_config: http_match(HttpMethod::Post, "/v1/embeddings"),
        plugins: None,
        rate_limit: None,
        tags: Vec::new(),
    };
    let replaced = service.replace_route(tenant, route.id, update).await.unwrap();
    assert!(!replaced.enabled);
    assert_eq!(replaced.priority, 5);
    assert_eq!(replaced.upstream_id, upstream.id, "upstream_id immutable");
    assert_eq!(
        replaced.match_config.http.as_ref().map(|http| http.path.as_str()),
        Some("/v1/embeddings")
    );

    service.delete_route(tenant, route.id).await.unwrap();
    assert!(matches!(
        service.get_route(tenant, route.id).await.unwrap_err(),
        DomainError::NotFound { .. }
    ));
}

#[tokio::test]
async fn route_requires_an_upstream_of_the_calling_tenant() {
    let service = service();
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let upstream = service
        .create_upstream(owner, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();

    // Ancestors are not directly addressable: a foreign upstream id yields 404,
    // not a partially-created route.
    let err = service
        .create_route(other, route_draft(upstream.id, "/v1/chat"))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::NotFound { .. }), "{err:?}");
    assert!(service.list_routes(other).await.unwrap().is_empty());
}

#[tokio::test]
async fn route_match_must_match_the_upstream_protocol() {
    let service = service();
    let tenant = Uuid::new_v4();
    let mut draft = upstream_draft(&["api.openai.com"]);
    draft.protocol = Protocol::Grpc;
    let upstream = service.create_upstream(tenant, draft).await.unwrap();

    let err = service
        .create_route(tenant, route_draft(upstream.id, "/v1/chat"))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::ValidationError { .. }), "{err:?}");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn duplicate_route_match_rule_is_rejected() {
    let service = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();
    service
        .create_route(tenant, route_draft(upstream.id, "/v1/chat"))
        .await
        .unwrap();

    let err = service
        .create_route(tenant, route_draft(upstream.id, "/v1/chat"))
        .await
        .unwrap_err();
    match err {
        DomainError::Conflict { invalid_value, .. } => {
            assert_eq!(invalid_value.as_deref(), Some("/v1/chat Get"));
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    // A different path, or a different priority, is a distinct match rule.
    service
        .create_route(tenant, route_draft(upstream.id, "/v1/embeddings"))
        .await
        .unwrap();
    let mut higher = route_draft(upstream.id, "/v1/chat");
    higher.priority = 1;
    service.create_route(tenant, higher).await.unwrap();
}

#[tokio::test]
async fn replace_route_collides_with_a_sibling_match() {
    let service = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();
    let first = service
        .create_route(tenant, route_draft(upstream.id, "/v1/chat"))
        .await
        .unwrap();
    let mut second = route_draft(upstream.id, "/v1/embeddings");
    second.priority = 1;
    let second = service.create_route(tenant, second).await.unwrap();

    // Point the second route at the first route's match rule.
    let update = RouteUpdate {
        enabled: true,
        priority: 0,
        match_config: http_match(HttpMethod::Get, "/v1/chat"),
        plugins: None,
        rate_limit: None,
        tags: Vec::new(),
    };
    let err = service
        .replace_route(tenant, second.id, update.clone())
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Conflict { .. }), "{err:?}");

    // The untouched sibling is unaffected, and replacing a route with its own
    // match rule is a no-op.
    let kept = service.replace_route(tenant, first.id, update).await.unwrap();
    assert_eq!(kept.id, first.id);
}

// ---------------------------------------------------------------------------
// plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_round_trip_and_source() {
    let service = service();
    let tenant = Uuid::new_v4();
    let plugin = service
        .create_plugin(tenant, plugin_draft("block-secrets"))
        .await
        .unwrap();
    assert_eq!(plugin.plugin_type, PluginKind::Guard);
    assert!(plugin.gts_id().starts_with("gts.cf.core.oagw.guard_plugin.v1~"));
    assert!(plugin.gc_eligible_at.is_some());

    let (loaded, source) = service.get_plugin_source(tenant, plugin.id).await.unwrap();
    assert_eq!(loaded.id, plugin.id);
    assert!(source.contains("guard_request"));

    service.delete_plugin(tenant, plugin.id).await.unwrap();
    assert!(matches!(
        service.get_plugin(tenant, plugin.id).await.unwrap_err(),
        DomainError::NotFound { .. }
    ));
}

#[tokio::test]
async fn plugin_rejects_empty_source_and_duplicate_name() {
    let service = service();
    let tenant = Uuid::new_v4();
    service
        .create_plugin(tenant, plugin_draft("block-secrets"))
        .await
        .unwrap();

    let mut empty = plugin_draft("empty");
    empty.source_code = "   ".to_owned();
    assert!(matches!(
        service.create_plugin(tenant, empty).await.unwrap_err(),
        DomainError::ValidationError { .. }
    ));

    let err = service
        .create_plugin(tenant, plugin_draft("block-secrets"))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Conflict { .. }), "{err:?}");
}

#[tokio::test]
async fn delete_plugin_in_use_reports_references() {
    let service = service();
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(tenant, upstream_draft(&["api.openai.com"]))
        .await
        .unwrap();
    let plugin = service
        .create_plugin(tenant, plugin_draft("block-secrets"))
        .await
        .unwrap();

    // Bind the plugin to the upstream and to a route.
    let mut draft = upstream_draft(&["api.openai.com"]);
    draft.plugins = Some(crate::domain::models::PluginsConfig {
        items: vec![plugin.id.to_string()],
        ..crate::domain::models::PluginsConfig::default()
    });
    service.replace_upstream(tenant, upstream.id, draft).await.unwrap();
    let mut route = route_draft(upstream.id, "/v1/chat");
    route.plugins = Some(crate::domain::models::PluginsConfig {
        items: vec![plugin.id.to_string()],
        ..crate::domain::models::PluginsConfig::default()
    });
    service.create_route(tenant, route).await.unwrap();

    let err = service.delete_plugin(tenant, plugin.id).await.unwrap_err();
    match err {
        DomainError::PluginInUse {
            plugin_id,
            referenced_by,
        } => {
            assert_eq!(plugin_id, plugin.gts_id());
            assert_eq!(referenced_by.upstreams.len(), 1);
            assert_eq!(referenced_by.routes.len(), 1);
        }
        other => panic!("expected PluginInUse, got {other:?}"),
    }

    // Removing the bindings releases the plugin.
    let mut unbound = upstream_draft(&["api.openai.com"]);
    unbound.plugins = Some(crate::domain::models::PluginsConfig::default());
    service.replace_upstream(tenant, upstream.id, unbound).await.unwrap();
    let route = service.list_routes(tenant).await.unwrap().pop().expect("route");
    let cleared = RouteUpdate {
        enabled: true,
        priority: 0,
        match_config: http_match(HttpMethod::Get, "/v1/chat"),
        plugins: Some(crate::domain::models::PluginsConfig::default()),
        rate_limit: None,
        tags: Vec::new(),
    };
    service.replace_route(tenant, route.id, cleared).await.unwrap();
    service.delete_plugin(tenant, plugin.id).await.unwrap();
}

#[tokio::test]
async fn tenant_scoping_for_plugins() {
    let service = service();
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();
    let plugin = service
        .create_plugin(tenant, plugin_draft("block-secrets"))
        .await
        .unwrap();

    assert!(service.list_plugins(other).await.unwrap().is_empty());
    assert!(matches!(
        service.get_plugin(other, plugin.id).await.unwrap_err(),
        DomainError::NotFound { .. }
    ));
    assert!(matches!(
        service.delete_plugin(other, plugin.id).await.unwrap_err(),
        DomainError::NotFound { .. }
    ));
}
