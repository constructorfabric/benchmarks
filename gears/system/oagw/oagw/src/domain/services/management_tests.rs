//! Control Plane behaviour: CRUD, validation, alias enforcement, tenant
//! scoping, alias shadowing and route matching.

use super::*;
use crate::domain::model::{
    BurstConfig, Endpoint, HttpMatch, PluginBinding, RateAlgorithm, RateScope, RateStrategy,
    RateWindow, Scheme, SharingMode, SustainedRate,
};
use crate::domain::services::tenancy::FlatHierarchy;
use crate::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};
use std::collections::HashMap;

/// Hierarchy driven by an explicit child → ancestors map.
struct ScriptedHierarchy(HashMap<Uuid, Vec<Uuid>>);

#[async_trait]
impl TenantHierarchy for ScriptedHierarchy {
    async fn ancestors(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> DomainResult<Vec<Uuid>> {
        Ok(self.0.get(&tenant_id).cloned().unwrap_or_default())
    }
}

fn context(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}

fn service(hierarchy: Arc<dyn TenantHierarchy>) -> ControlPlaneServiceImpl {
    ControlPlaneServiceImpl::new(
        Arc::new(InMemoryUpstreamRepo::new()),
        Arc::new(InMemoryRouteRepo::new()),
        Arc::new(InMemoryPluginRepo::new()),
        hierarchy,
        30 * 24 * 60 * 60,
        true,
    )
}

fn flat_service() -> ControlPlaneServiceImpl {
    service(Arc::new(FlatHierarchy))
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: host.to_owned(),
        port: Some(port),
    }
}

fn upstream_spec(hosts: &[&str], alias: Option<&str>) -> UpstreamSpec {
    UpstreamSpec {
        alias: alias.map(str::to_owned),
        enabled: None,
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: hosts.iter().map(|h| endpoint(h, 443)).collect(),
        },
        protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn route_spec(upstream_id: Uuid, methods: &[&str], path: &str) -> RouteSpec {
    RouteSpec {
        upstream_id: Some(upstream_id),
        enabled: None,
        priority: None,
        tags: Vec::new(),
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn plugin_spec(name: &str, kind: PluginKind) -> PluginSpec {
    PluginSpec {
        name: name.to_owned(),
        description: None,
        plugin_type: kind,
        phases: vec![PluginPhase::OnRequest],
        config_schema: serde_json::json!({ "type": "object" }),
        source_code: "def on_request(ctx):\n    return ctx.next()".to_owned(),
    }
}

fn rate_limit(rate: u32, sharing: SharingMode) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: RateWindow::Minute,
        },
        burst: BurstConfig { capacity: None },
        budget: None,
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

// ---------------------------------------------------------------------------
// Upstream CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_derives_the_alias_from_a_hostname_pool() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    assert_eq!(upstream.alias, "api.openai.com");
    assert!(upstream.enabled, "enabled defaults to true");
}

#[tokio::test]
async fn create_rejects_a_user_alias_on_a_hostname_pool() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let err = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], Some("openai")))
        .await
        .expect_err("alias is derived, not chosen");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn create_requires_an_alias_for_an_ip_pool() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    assert_eq!(
        svc.create_upstream(&ctx, upstream_spec(&["10.0.1.1", "10.0.1.2"], None))
            .await
            .expect_err("alias required")
            .status(),
        400
    );
    let upstream = svc
        .create_upstream(
            &ctx,
            upstream_spec(&["10.0.1.1", "10.0.1.2"], Some("my-service")),
        )
        .await
        .expect("created");
    assert_eq!(upstream.alias, "my-service");
}

#[tokio::test]
async fn alias_is_unique_per_tenant_and_conflicts_are_409() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    svc.create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("first");
    let err = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect_err("duplicate alias");
    assert_eq!(err.status(), 409);
}

#[tokio::test]
async fn another_tenant_may_shadow_the_same_alias() {
    let svc = flat_service();
    svc.create_upstream(
        &context(Uuid::new_v4()),
        upstream_spec(&["api.openai.com"], None),
    )
    .await
    .expect("tenant a");
    svc.create_upstream(
        &context(Uuid::new_v4()),
        upstream_spec(&["api.openai.com"], None),
    )
    .await
    .expect("tenant b may shadow");
}

#[tokio::test]
async fn replace_rejects_an_endpoint_change_that_would_move_the_alias() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    let err = svc
        .replace_upstream(
            &ctx,
            upstream.id,
            upstream_spec(&["api.anthropic.com"], None),
        )
        .await
        .expect_err("alias is immutable");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn replace_allows_an_endpoint_change_that_keeps_the_alias() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(
            &ctx,
            upstream_spec(&["us.vendor.com", "eu.vendor.com"], None),
        )
        .await
        .expect("created");
    assert_eq!(upstream.alias, "vendor.com");
    let replaced = svc
        .replace_upstream(
            &ctx,
            upstream.id,
            upstream_spec(&["us.vendor.com", "eu.vendor.com", "apac.vendor.com"], None),
        )
        .await
        .expect("alias unchanged");
    assert_eq!(replaced.alias, "vendor.com");
    assert_eq!(replaced.server.endpoints.len(), 3);
}

#[tokio::test]
async fn reads_and_writes_are_tenant_scoped() {
    let svc = flat_service();
    let owner = context(Uuid::new_v4());
    let other = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&owner, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");

    assert_eq!(
        svc.get_upstream(&other, upstream.id)
            .await
            .expect_err("invisible")
            .status(),
        404
    );
    assert_eq!(
        svc.delete_upstream(&other, upstream.id)
            .await
            .expect_err("invisible")
            .status(),
        404
    );
    assert!(svc.list_upstreams(&other).await.expect("list").is_empty());
    assert!(svc.get_upstream(&owner, upstream.id).await.is_ok());
}

#[tokio::test]
async fn deleting_an_upstream_cascades_to_its_routes() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    svc.create_route(&ctx, route_spec(upstream.id, &["GET"], "/v1/models"))
        .await
        .expect("route");
    svc.delete_upstream(&ctx, upstream.id)
        .await
        .expect("deleted");
    assert!(svc.list_routes(&ctx).await.expect("list").is_empty());
}

#[tokio::test]
async fn an_unknown_protocol_is_rejected() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let mut spec = upstream_spec(&["api.openai.com"], None);
    spec.protocol = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.carrier_pigeon.v1".to_owned();
    assert_eq!(
        svc.create_upstream(&ctx, spec)
            .await
            .expect_err("unknown protocol")
            .status(),
        400
    );
}

#[tokio::test]
async fn catalog_only_plugin_identifiers_cannot_be_bound() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());

    let mut spec = upstream_spec(&["api.openai.com"], None);
    spec.auth = Some(AuthConfig {
        plugin_ref: Some(gts_helpers::BEARER_AUTH_PLUGIN_ID.to_owned()),
        sharing: SharingMode::Private,
        config: serde_json::Map::new(),
    });
    assert_eq!(
        svc.create_upstream(&ctx, spec)
            .await
            .expect_err("reserved identifier")
            .status(),
        400
    );

    let mut spec = upstream_spec(&["api.anthropic.com"], None);
    spec.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding::named(gts_helpers::CORS_GUARD_PLUGIN_ID)],
    });
    assert_eq!(
        svc.create_upstream(&ctx, spec)
            .await
            .expect_err("catalog-only guard")
            .status(),
        400
    );
}

#[tokio::test]
async fn credentials_with_a_wildcard_cors_origin_are_rejected() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let mut spec = upstream_spec(&["api.openai.com"], None);
    spec.cors = Some(CorsConfig {
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allow_credentials: true,
        ..CorsConfig::default()
    });
    assert_eq!(
        svc.create_upstream(&ctx, spec)
            .await
            .expect_err("insecure combination")
            .status(),
        400
    );
}

// ---------------------------------------------------------------------------
// Route CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_route_must_name_an_upstream_of_the_calling_tenant() {
    let svc = flat_service();
    let owner = context(Uuid::new_v4());
    let other = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&owner, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    assert_eq!(
        svc.create_route(&other, route_spec(upstream.id, &["GET"], "/v1"))
            .await
            .expect_err("not addressable")
            .status(),
        400
    );
}

#[tokio::test]
async fn duplicate_match_rules_conflict() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    svc.create_route(&ctx, route_spec(upstream.id, &["GET", "POST"], "/v1"))
        .await
        .expect("first");
    let err = svc
        .create_route(&ctx, route_spec(upstream.id, &["POST"], "/v1"))
        .await
        .expect_err("same path, priority and method");
    assert_eq!(err.status(), 409);

    // A different priority disambiguates.
    let mut spec = route_spec(upstream.id, &["POST"], "/v1");
    spec.priority = Some(10);
    svc.create_route(&ctx, spec).await.expect("distinct priority");
}

#[tokio::test]
async fn route_upstream_id_is_immutable_across_a_replace() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let first = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    let second = svc
        .create_upstream(&ctx, upstream_spec(&["api.anthropic.com"], None))
        .await
        .expect("created");
    let route = svc
        .create_route(&ctx, route_spec(first.id, &["GET"], "/v1"))
        .await
        .expect("route");

    let mut spec = route_spec(second.id, &["GET"], "/v2");
    spec.upstream_id = Some(second.id);
    let replaced = svc
        .replace_route(&ctx, route.id, spec)
        .await
        .expect("replaced");
    assert_eq!(replaced.upstream_id, first.id);
}

#[tokio::test]
async fn a_route_match_must_pick_exactly_one_protocol() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");

    let mut spec = route_spec(upstream.id, &["GET"], "/v1");
    spec.match_config.grpc = Some(crate::domain::model::GrpcMatch {
        service: "foo.v1.Bar".to_owned(),
        method: "Baz".to_owned(),
    });
    assert_eq!(
        svc.create_route(&ctx, spec)
            .await
            .expect_err("both")
            .status(),
        400
    );

    let mut spec = route_spec(upstream.id, &["GET"], "/v1");
    spec.match_config = MatchConfig::default();
    assert_eq!(
        svc.create_route(&ctx, spec)
            .await
            .expect_err("neither")
            .status(),
        400
    );
}

#[tokio::test]
async fn route_paths_must_be_absolute_and_traversal_free() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    for bad in ["v1/models", "/v1/../../etc"] {
        assert_eq!(
            svc.create_route(&ctx, route_spec(upstream.id, &["GET"], bad))
                .await
                .expect_err("rejected")
                .status(),
            400,
            "{bad} should be rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_crud_and_source_retrieval() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let plugin = svc
        .create_plugin(&ctx, plugin_spec("validator", PluginKind::Guard))
        .await
        .expect("created");
    assert!(plugin.gts_id().starts_with(gts_helpers::GUARD_PLUGIN_TYPE));

    let fetched = svc.get_plugin(&ctx, plugin.id).await.expect("fetched");
    assert_eq!(fetched.source_code, plugin.source_code);
    assert_eq!(svc.list_plugins(&ctx).await.expect("list").len(), 1);

    svc.delete_plugin(&ctx, plugin.id).await.expect("deleted");
    assert_eq!(
        svc.get_plugin(&ctx, plugin.id)
            .await
            .expect_err("gone")
            .status(),
        404
    );
}

#[tokio::test]
async fn a_bound_plugin_cannot_be_deleted() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let plugin = svc
        .create_plugin(&ctx, plugin_spec("validator", PluginKind::Guard))
        .await
        .expect("created");
    let plugin_ref = gts_helpers::anonymous_id(gts_helpers::GUARD_PLUGIN_TYPE, plugin.id);

    let mut spec = upstream_spec(&["api.openai.com"], None);
    spec.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding::named(plugin_ref)],
    });
    let upstream = svc.create_upstream(&ctx, spec).await.expect("bound");

    let err = svc
        .delete_plugin(&ctx, plugin.id)
        .await
        .expect_err("still bound");
    assert_eq!(err.status(), 409);
    assert_eq!(err.gts_type(), gts_helpers::errors::PLUGIN_IN_USE);
    assert!(err.extensions().contains_key("referenced_by"));

    // Unbinding releases it.
    svc.delete_upstream(&ctx, upstream.id)
        .await
        .expect("deleted");
    svc.delete_plugin(&ctx, plugin.id).await.expect("now free");
}

#[tokio::test]
async fn binding_a_plugin_of_the_wrong_kind_is_rejected() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let plugin = svc
        .create_plugin(&ctx, plugin_spec("transformer", PluginKind::Transform))
        .await
        .expect("created");
    // Reference the transform definition under the guard base type.
    let wrong = gts_helpers::anonymous_id(gts_helpers::GUARD_PLUGIN_TYPE, plugin.id);

    let mut spec = upstream_spec(&["api.openai.com"], None);
    spec.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding::named(wrong)],
    });
    assert_eq!(
        svc.create_upstream(&ctx, spec)
            .await
            .expect_err("kind mismatch")
            .status(),
        400
    );
}

#[tokio::test]
async fn binding_another_tenants_plugin_is_rejected() {
    let svc = flat_service();
    let owner = context(Uuid::new_v4());
    let other = context(Uuid::new_v4());
    let plugin = svc
        .create_plugin(&owner, plugin_spec("validator", PluginKind::Guard))
        .await
        .expect("created");
    let plugin_ref = gts_helpers::anonymous_id(gts_helpers::GUARD_PLUGIN_TYPE, plugin.id);

    let mut spec = upstream_spec(&["api.openai.com"], None);
    spec.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding::named(plugin_ref)],
    });
    assert_eq!(
        svc.create_upstream(&other, spec)
            .await
            .expect_err("cross-tenant")
            .status(),
        400
    );
}

#[tokio::test]
async fn an_unlinked_plugin_becomes_gc_eligible() {
    let svc = ControlPlaneServiceImpl::new(
        Arc::new(InMemoryUpstreamRepo::new()),
        Arc::new(InMemoryRouteRepo::new()),
        Arc::new(InMemoryPluginRepo::new()),
        Arc::new(FlatHierarchy),
        // Zero TTL: anything unlinked is collectable on the next sweep.
        0,
        true,
    );
    let ctx = context(Uuid::new_v4());
    let plugin = svc
        .create_plugin(&ctx, plugin_spec("ephemeral", PluginKind::Guard))
        .await
        .expect("created");
    assert!(plugin.gc_eligible_at.is_some());

    // Any write path runs the sweep; creating an upstream is one.
    svc.create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    assert_eq!(
        svc.get_plugin(&ctx, plugin.id)
            .await
            .expect_err("collected")
            .status(),
        404
    );
}

// ---------------------------------------------------------------------------
// Proxy-target resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolution_finds_the_upstream_and_the_matching_route() {
    let svc = flat_service();
    let tenant = Uuid::new_v4();
    let ctx = context(tenant);
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    svc.create_route(
        &ctx,
        route_spec(upstream.id, &["GET", "POST"], "/v1/chat/completions"),
    )
    .await
    .expect("route");

    let target = svc
        .resolve_proxy_target(&ctx, "API.OpenAI.com", "POST", Some("v1/chat/completions"))
        .await
        .expect("resolved");
    assert_eq!(target.upstream.id, upstream.id);
    assert!(target.route.is_some());
}

#[tokio::test]
async fn an_unknown_alias_is_a_404() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let err = svc
        .resolve_proxy_target(&ctx, "absent", "GET", None)
        .await
        .expect_err("unknown alias");
    assert_eq!(err.status(), 404);
    assert_eq!(err.gts_type(), gts_helpers::errors::ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let mut spec = upstream_spec(&["api.openai.com"], None);
    spec.enabled = Some(false);
    svc.create_upstream(&ctx, spec).await.expect("created");
    let err = svc
        .resolve_proxy_target(&ctx, "api.openai.com", "GET", None)
        .await
        .expect_err("disabled");
    assert_eq!(err.status(), 503);
}

#[tokio::test]
async fn a_descendant_shadows_an_ancestor_alias() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let svc = service(Arc::new(ScriptedHierarchy(HashMap::from([(
        leaf,
        vec![root],
    )]))));

    let root_upstream = svc
        .create_upstream(&context(root), upstream_spec(&["api.openai.com"], None))
        .await
        .expect("root upstream");
    let leaf_upstream = svc
        .create_upstream(&context(leaf), upstream_spec(&["api.openai.com"], None))
        .await
        .expect("leaf upstream");

    let target = svc
        .resolve_proxy_target(&context(leaf), "api.openai.com", "GET", None)
        .await
        .expect("resolved");
    assert_eq!(
        target.upstream.id, leaf_upstream.id,
        "the closest tenant wins"
    );
    assert_ne!(target.upstream.id, root_upstream.id);
}

#[tokio::test]
async fn a_descendant_inherits_an_ancestor_upstream_and_its_routes() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let svc = service(Arc::new(ScriptedHierarchy(HashMap::from([(
        leaf,
        vec![root],
    )]))));

    let root_upstream = svc
        .create_upstream(&context(root), upstream_spec(&["api.openai.com"], None))
        .await
        .expect("root upstream");
    svc.create_route(
        &context(root),
        route_spec(root_upstream.id, &["GET"], "/v1/models"),
    )
    .await
    .expect("root route");

    let target = svc
        .resolve_proxy_target(&context(leaf), "api.openai.com", "GET", Some("v1/models"))
        .await
        .expect("inherited");
    assert_eq!(target.upstream.id, root_upstream.id);
    assert!(target.route.is_some());
}

#[tokio::test]
async fn an_ancestor_disabling_an_upstream_disables_it_for_descendants() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let svc = service(Arc::new(ScriptedHierarchy(HashMap::from([(
        leaf,
        vec![root],
    )]))));

    let mut disabled = upstream_spec(&["api.openai.com"], None);
    disabled.enabled = Some(false);
    svc.create_upstream(&context(root), disabled)
        .await
        .expect("root upstream");
    // The descendant tries to re-enable it under the same alias.
    svc.create_upstream(&context(leaf), upstream_spec(&["api.openai.com"], None))
        .await
        .expect("leaf upstream");

    let err = svc
        .resolve_proxy_target(&context(leaf), "api.openai.com", "GET", None)
        .await
        .expect_err("ancestor wins");
    assert_eq!(err.status(), 503);
}

#[tokio::test]
async fn an_enforced_ancestor_rate_limit_survives_shadowing() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let svc = service(Arc::new(ScriptedHierarchy(HashMap::from([(
        leaf,
        vec![root],
    )]))));

    let mut root_spec = upstream_spec(&["api.openai.com"], None);
    root_spec.rate_limit = Some(rate_limit(100, SharingMode::Enforce));
    svc.create_upstream(&context(root), root_spec)
        .await
        .expect("root upstream");

    let mut leaf_spec = upstream_spec(&["api.openai.com"], None);
    leaf_spec.rate_limit = Some(rate_limit(50_000, SharingMode::Private));
    svc.create_upstream(&context(leaf), leaf_spec)
        .await
        .expect("leaf upstream");

    let target = svc
        .resolve_proxy_target(&context(leaf), "api.openai.com", "GET", None)
        .await
        .expect("resolved");
    let effective = target
        .rate_limits
        .first()
        .expect("an upstream-level limit is in force");
    assert_eq!(effective.config.sustained.rate, 100);
}

#[tokio::test]
async fn upstream_plugins_run_before_route_plugins() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());

    let mut spec = upstream_spec(&["api.openai.com"], None);
    spec.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding::named(
            gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
        )],
    });
    let upstream = svc.create_upstream(&ctx, spec).await.expect("created");

    let mut route = route_spec(upstream.id, &["GET"], "/v1");
    route.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding::named(
            gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        )],
    });
    svc.create_route(&ctx, route).await.expect("route");

    let target = svc
        .resolve_proxy_target(&ctx, "api.openai.com", "GET", Some("v1/models"))
        .await
        .expect("resolved");
    let refs: Vec<&str> = target
        .plugins
        .items
        .iter()
        .map(|b| b.plugin_ref.as_str())
        .collect();
    assert_eq!(
        refs,
        vec![
            gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
            gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID
        ],
        "a `private` upstream chain still composes with its own route's chain"
    );
}

#[tokio::test]
async fn the_longest_path_prefix_wins_then_priority() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    let broad = svc
        .create_route(&ctx, route_spec(upstream.id, &["GET"], "/v1"))
        .await
        .expect("broad");
    let narrow = svc
        .create_route(&ctx, route_spec(upstream.id, &["GET"], "/v1/chat"))
        .await
        .expect("narrow");

    let target = svc
        .resolve_proxy_target(&ctx, "api.openai.com", "GET", Some("v1/chat/completions"))
        .await
        .expect("resolved");
    assert_eq!(target.route.map(|r| r.id), Some(narrow.id));

    let target = svc
        .resolve_proxy_target(&ctx, "api.openai.com", "GET", Some("v1/models"))
        .await
        .expect("resolved");
    assert_eq!(target.route.map(|r| r.id), Some(broad.id));
}

#[tokio::test]
async fn a_disabled_route_is_excluded_from_matching() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    let mut spec = route_spec(upstream.id, &["GET"], "/v1");
    spec.enabled = Some(false);
    svc.create_route(&ctx, spec).await.expect("route");

    let target = svc
        .resolve_proxy_target(&ctx, "api.openai.com", "GET", Some("v1/models"))
        .await
        .expect("upstream still resolves");
    assert!(target.route.is_none());
}

#[tokio::test]
async fn path_prefixes_match_on_segment_boundaries_only() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&ctx, upstream_spec(&["api.openai.com"], None))
        .await
        .expect("created");
    svc.create_route(&ctx, route_spec(upstream.id, &["GET"], "/v1/chat"))
        .await
        .expect("route");

    let target = svc
        .resolve_proxy_target(&ctx, "api.openai.com", "GET", Some("v1/chatter"))
        .await
        .expect("resolved");
    assert!(
        target.route.is_none(),
        "/v1/chat must not swallow /v1/chatter"
    );
}

#[tokio::test]
async fn a_grpc_upstream_is_not_proxied_in_this_build() {
    let svc = flat_service();
    let ctx = context(Uuid::new_v4());
    let mut spec = upstream_spec(&["grpc.example.com"], None);
    spec.protocol = gts_helpers::PROTOCOL_GRPC.to_owned();
    svc.create_upstream(&ctx, spec).await.expect("created");
    let err = svc
        .resolve_proxy_target(&ctx, "grpc.example.com", "POST", Some("foo.v1.Bar/Baz"))
        .await
        .expect_err("not implemented");
    assert_eq!(err.status(), 501);
}

// ---------------------------------------------------------------------------
// Outbound path construction
// ---------------------------------------------------------------------------

fn route_for(path: &str, mode: PathSuffixMode) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        upstream_id: Uuid::new_v4(),
        enabled: true,
        priority: 0,
        tags: Vec::new(),
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: mode,
            }),
            grpc: None,
        },
        plugins: PluginsConfig::default(),
        rate_limit: None,
        cors: None,
    }
}

#[test]
fn append_mode_joins_the_suffix_onto_the_route_path() {
    let route = route_for("/v1", PathSuffixMode::Append);
    assert_eq!(
        outbound_path(&route, Some("v1/chat/completions")).expect("path"),
        "/v1/chat/completions"
    );
    assert_eq!(outbound_path(&route, None).expect("path"), "/v1");
}

#[test]
fn disabled_mode_rejects_an_extra_suffix_but_tolerates_the_route_path() {
    let route = route_for("/v1/models", PathSuffixMode::Disabled);
    assert_eq!(
        outbound_path(&route, Some("v1/models")).expect("exact"),
        "/v1/models"
    );
    let err = outbound_path(&route, Some("v1/models/extra")).expect_err("rejected");
    assert_eq!(err.status(), 400);
}

#[test]
fn a_root_route_forwards_the_whole_suffix() {
    let route = route_for("/", PathSuffixMode::Append);
    assert_eq!(
        outbound_path(&route, Some("anything/at/all")).expect("path"),
        "/anything/at/all"
    );
}
