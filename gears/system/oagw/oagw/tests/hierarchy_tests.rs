// Hierarchical configuration integration tests: auth / plugin / CORS /
// rate-limit merge across the tenant chain, enable-disable inheritance,
// permission checks and create-time validation.
//
// The helpers are local to this file on purpose: `tests/common/mod.rs` is
// shared with the other integration tests and is deliberately untouched.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::{
    AuthZResolverClient, AuthZResolverError, DenyReason, EvaluationRequest, EvaluationResponse,
    EvaluationResponseContext,
};
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantResolverClient,
    TenantResolverError,
};
use toolkit_security::SecurityContext;

use common::{StaticTenants, context_for};
use oagw::domain::error::DomainError;
use oagw::domain::gts;
use oagw::domain::model::{
    AuthConfig, CorsConfig, Endpoint, EndpointScheme, PluginBinding, PluginsConfig,
    RateLimitConfig, Route, ServerConfig, SharingMode, SustainedRate, Upstream,
};
use oagw::domain::services::management::{ControlPlaneService, ListQuery};
use oagw::infra::controlplane::{ControlPlaneServiceImpl, UPSTREAM_DISABLED_RETRY_AFTER_SECS};
use oagw::infra::storage::InMemoryStore;

/// Deny reason of the scripted PDP.
const DENY_DETAILS: &str = "the subject lacks the required role";

/// An `authz-resolver` stub with a scriptable decision.
struct ScriptedAuthZ {
    /// Actions that are denied; everything else is allowed.
    denied: Vec<&'static str>,
}

#[async_trait]
impl AuthZResolverClient for ScriptedAuthZ {
    async fn evaluate(
        &self,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        let decision = !self.denied.contains(&request.action.name.as_str());
        Ok(EvaluationResponse {
            decision,
            context: EvaluationResponseContext {
                deny_reason: (!decision).then(|| DenyReason {
                    error_code: "policy.denied".to_owned(),
                    details: Some(DENY_DETAILS.to_owned()),
                }),
                ..EvaluationResponseContext::default()
            },
        })
    }
}

/// A tenant resolver whose lookups always fail, for the fail-closed test.
struct BrokenTenants;

const UNAVAILABLE: &str = "unavailable in the hierarchy tests";

#[async_trait]
impl TenantResolverClient for BrokenTenants {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::ServiceUnavailable(
            UNAVAILABLE.to_owned(),
        ))
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::ServiceUnavailable(
            UNAVAILABLE.to_owned(),
        ))
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Err(TenantResolverError::ServiceUnavailable(
            UNAVAILABLE.to_owned(),
        ))
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        Err(TenantResolverError::ServiceUnavailable(
            UNAVAILABLE.to_owned(),
        ))
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Err(TenantResolverError::ServiceUnavailable(
            UNAVAILABLE.to_owned(),
        ))
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor_id: TenantId,
        _descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Err(TenantResolverError::ServiceUnavailable(
            UNAVAILABLE.to_owned(),
        ))
    }
}

/// The control plane plus the `root -> child` hierarchy of the test.
struct Fixture {
    control_plane: Arc<ControlPlaneServiceImpl>,
    root: TenantId,
    child: TenantId,
}

impl Fixture {
    fn root_ctx(&self) -> SecurityContext {
        context_for(self.root)
    }

    fn child_ctx(&self) -> SecurityContext {
        context_for(self.child)
    }

    fn control_plane(&self) -> &Arc<ControlPlaneServiceImpl> {
        &self.control_plane
    }
}

/// A `root -> child` hierarchy with no permission enforcement.
fn fixture() -> Fixture {
    fixture_with(Vec::new())
}

/// A `root -> child` hierarchy with a PDP denying the listed actions.
fn fixture_with(denied: Vec<&'static str>) -> Fixture {
    let root = TenantId(uuid::Uuid::new_v4());
    let child = TenantId(uuid::Uuid::new_v4());
    let tenants: Arc<dyn TenantResolverClient> = Arc::new(StaticTenants::root_child(root, child));
    let authz: Arc<dyn AuthZResolverClient> = Arc::new(ScriptedAuthZ { denied });
    let control_plane = Arc::new(
        ControlPlaneServiceImpl::new(InMemoryStore::new(), Some(tenants)).with_authz(Some(authz)),
    );
    Fixture {
        control_plane,
        root,
        child,
    }
}

/// An HTTPS endpoint whose hostname derives `alias`.
fn endpoint(host: &str) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port: 443,
    }
}

/// An upstream shell for `tenant_id` on `host`, alias derived from the host.
fn upstream(tenant_id: uuid::Uuid, host: &str) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id,
        alias: String::new(),
        protocol: gts::PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: ServerConfig {
            endpoints: vec![endpoint(host)],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: 0,
    }
}

/// A rate limit of `rate` tokens per second with the given sharing mode.
fn rate_limit(sharing: SharingMode, rate: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: oagw::domain::model::RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: oagw::domain::model::RateWindow::default(),
        },
        burst: None,
        scope: oagw::domain::model::RateScope::Tenant,
        strategy: oagw::domain::model::RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

/// An auth binding of `auth_type` with the given sharing mode.
fn auth(sharing: SharingMode, auth_type: &str) -> AuthConfig {
    AuthConfig {
        auth_type: Some(auth_type.to_owned()),
        sharing,
        config: Some(serde_json::json!({ "header": "authorization" })),
    }
}

/// A plugin chain of bare references with the given sharing mode.
fn plugins(sharing: SharingMode, items: Vec<&str>) -> PluginsConfig {
    PluginsConfig {
        sharing,
        items: items
            .into_iter()
            .map(|reference| PluginBinding::Bare(reference.to_owned()))
            .collect(),
    }
}

/// A CORS configuration with the given sharing mode and origins.
fn cors(sharing: SharingMode, origins: &[&str]) -> CorsConfig {
    CorsConfig {
        sharing,
        enabled: true,
        allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        expose_headers: vec!["x-request-id".to_owned()],
        allow_credentials: false,
    }
}

/// A route on `upstream_id` owned by `tenant_id`.
fn route(tenant: uuid::Uuid, upstream_id: uuid::Uuid, methods: &[&str]) -> Route {
    let mut route = common::http_route(upstream_id, "/v1", methods);
    route.tenant_id = tenant;
    route
}

/// Creates `payload` and a `GET /v1` route for it, as `ctx`.
///
/// Returns the created upstream, whose alias is derived from its endpoints.
async fn create_upstream_with_route(
    fixture: &Fixture,
    ctx: &SecurityContext,
    payload: Upstream,
) -> Upstream {
    let created = fixture
        .control_plane()
        .create_upstream(ctx, payload)
        .await
        .expect("upstream");
    fixture
        .control_plane()
        .create_route(ctx, route(ctx.subject_tenant_id(), created.id, &["GET"]))
        .await
        .expect("route");
    created
}

/// Resolves the `GET /v1/items` target of `alias` as the child tenant.
async fn resolve(
    fixture: &Fixture,
    alias: &str,
) -> Result<oagw::domain::services::management::ResolvedTarget, DomainError> {
    fixture
        .control_plane()
        .resolve_proxy_target(&fixture.child_ctx(), alias, "GET", "/v1/items", "")
        .await
}

#[tokio::test]
async fn a_descendant_auth_override_beats_an_inherit_ancestor() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.auth = Some(auth(SharingMode::Inherit, gts::AUTH_PLUGIN_NOOP));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    let mut child_upstream = upstream(fixture.child.0, "api.vendor.com");
    child_upstream.auth = Some(auth(SharingMode::Private, gts::AUTH_PLUGIN_APIKEY));
    create_upstream_with_route(&fixture, &fixture.child_ctx(), child_upstream).await;

    let target = resolve(&fixture, "api.vendor.com").await.expect("target");
    assert_eq!(
        target
            .plugin_chain
            .auth
            .as_ref()
            .map(|invocation| invocation.plugin_ref.as_str()),
        Some(gts::AUTH_PLUGIN_APIKEY),
        "the closest tenant's own auth wins over the inherited one"
    );
}

#[tokio::test]
async fn an_inherited_auth_applies_when_the_descendant_declares_none() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.auth = Some(auth(SharingMode::Inherit, gts::AUTH_PLUGIN_APIKEY));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    create_upstream_with_route(
        &fixture,
        &fixture.child_ctx(),
        upstream(fixture.child.0, "api.vendor.com"),
    )
    .await;

    let target = resolve(&fixture, "api.vendor.com").await.expect("target");
    assert_eq!(
        target
            .plugin_chain
            .auth
            .as_ref()
            .map(|invocation| invocation.plugin_ref.as_str()),
        Some(gts::AUTH_PLUGIN_APIKEY),
        "the inherited auth is the fallback for an unauthenticated descendant"
    );
    assert!(
        !target.inherited,
        "the closest upstream belongs to the calling tenant"
    );
}

#[tokio::test]
async fn inherit_ancestor_plugins_are_concatenated_with_the_descendant_chain() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.plugins = Some(plugins(
        SharingMode::Inherit,
        vec![gts::TRANSFORM_PLUGIN_REQUEST_ID],
    ));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    let mut child_upstream = upstream(fixture.child.0, "api.vendor.com");
    child_upstream.plugins = Some(plugins(
        SharingMode::Private,
        vec![gts::GUARD_PLUGIN_REQUIRED_HEADERS],
    ));
    create_upstream_with_route(&fixture, &fixture.child_ctx(), child_upstream).await;

    let target = resolve(&fixture, "api.vendor.com").await.expect("target");
    let transforms: Vec<&str> = target
        .plugin_chain
        .transforms
        .iter()
        .map(|invocation| invocation.plugin_ref.as_str())
        .collect();
    assert_eq!(
        transforms,
        vec![gts::TRANSFORM_PLUGIN_REQUEST_ID],
        "the inherited upstream transform plugin is carried over"
    );
    let guards: Vec<&str> = target
        .plugin_chain
        .guards
        .iter()
        .map(|invocation| invocation.plugin_ref.as_str())
        .collect();
    assert_eq!(
        guards,
        vec![gts::GUARD_PLUGIN_REQUIRED_HEADERS],
        "the descendant's own guard plugin is appended"
    );
}

#[tokio::test]
async fn an_enforce_ancestor_cors_is_not_widened_by_the_descendant() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.cors = Some(cors(SharingMode::Enforce, &["https://root.example"]));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    let mut child_upstream = upstream(fixture.child.0, "api.vendor.com");
    child_upstream.cors = Some(cors(SharingMode::Private, &["https://child.example"]));
    create_upstream_with_route(&fixture, &fixture.child_ctx(), child_upstream).await;

    let target = resolve(&fixture, "api.vendor.com").await.expect("target");
    let effective = target.cors.expect("the ancestor CORS policy applies");
    assert_eq!(
        effective.allowed_origins,
        vec!["https://root.example".to_owned()],
        "the enforced ancestor origins are authoritative"
    );
    assert_eq!(effective.sharing, SharingMode::Enforce);
    assert!(
        effective.enabled,
        "an enforced ancestor cannot be switched off"
    );
}

#[tokio::test]
async fn an_inherit_ancestor_cors_is_unioned_with_the_descendant_origins() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.cors = Some(cors(SharingMode::Inherit, &["https://root.example"]));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    let mut child_upstream = upstream(fixture.child.0, "api.vendor.com");
    child_upstream.cors = Some(cors(SharingMode::Private, &["https://child.example"]));
    create_upstream_with_route(&fixture, &fixture.child_ctx(), child_upstream).await;

    let target = resolve(&fixture, "api.vendor.com").await.expect("target");
    let effective = target.cors.expect("the inherited origins are unioned");
    let mut origins = effective.allowed_origins.clone();
    origins.sort();
    assert_eq!(
        origins,
        vec![
            "https://child.example".to_owned(),
            "https://root.example".to_owned(),
        ]
    );
}

#[tokio::test]
async fn an_inherit_ancestor_cors_applies_without_a_descendant_policy() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.cors = Some(cors(SharingMode::Inherit, &["https://root.example"]));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    create_upstream_with_route(
        &fixture,
        &fixture.child_ctx(),
        upstream(fixture.child.0, "api.vendor.com"),
    )
    .await;

    let target = resolve(&fixture, "api.vendor.com").await.expect("target");
    let effective = target.cors.expect("the inherited policy is the baseline");
    assert_eq!(
        effective.allowed_origins,
        vec!["https://root.example".to_owned()]
    );
}

#[tokio::test]
async fn an_inherit_ancestor_rate_limit_is_the_descendant_default() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.rate_limit = Some(rate_limit(SharingMode::Inherit, 50));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    create_upstream_with_route(
        &fixture,
        &fixture.child_ctx(),
        upstream(fixture.child.0, "api.vendor.com"),
    )
    .await;

    let target = resolve(&fixture, "api.vendor.com").await.expect("target");
    let effective = target.rate_limit.expect("the inherited limit applies");
    assert_eq!(effective.sustained.rate, 50);
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503_with_a_retry_after() {
    let fixture = fixture();
    let mut child_upstream = upstream(fixture.child.0, "api.vendor.com");
    child_upstream.enabled = false;
    create_upstream_with_route(&fixture, &fixture.child_ctx(), child_upstream).await;

    let error = resolve(&fixture, "api.vendor.com")
        .await
        .expect_err("the alias is disabled");
    let DomainError::UpstreamDisabled {
        alias,
        retry_after_seconds,
    } = &error
    else {
        panic!("expected UpstreamDisabled, got {error:?}");
    };
    assert_eq!(alias, "api.vendor.com");
    assert_eq!(*retry_after_seconds, UPSTREAM_DISABLED_RETRY_AFTER_SECS);
    assert_eq!(error.status(), 503);
    assert!(!error.is_client_error(), "a disabled upstream is a 503");
    let descriptor = error.descriptor();
    assert_eq!(
        descriptor.type_id,
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.disabled.v1"
    );
    assert_eq!(descriptor.title, "Upstream Disabled");
    assert_eq!(descriptor.error_code, "UPSTREAM_DISABLED");
    assert_eq!(
        descriptor.retry_after_seconds,
        Some(UPSTREAM_DISABLED_RETRY_AFTER_SECS)
    );
}

#[tokio::test]
async fn an_ancestor_disabled_alias_is_disabled_for_the_descendant() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.enabled = false;
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    create_upstream_with_route(
        &fixture,
        &fixture.child_ctx(),
        upstream(fixture.child.0, "api.vendor.com"),
    )
    .await;

    let error = resolve(&fixture, "api.vendor.com")
        .await
        .expect_err("an ancestor-disabled alias");
    assert!(
        matches!(error, DomainError::UpstreamDisabled { .. }),
        "{error:?}"
    );
    assert_eq!(error.status(), 503);
}

#[tokio::test]
async fn an_enforce_ancestor_rate_limit_applies_to_the_descendant() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.rate_limit = Some(rate_limit(SharingMode::Enforce, 5));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    // The descendant declares no upstream of its own: the ancestor's alias is
    // reachable from the descendant and its `enforce` limit is not negotiable.
    let target = resolve(&fixture, "api.vendor.com").await.expect("target");
    let effective = target.rate_limit.expect("the ancestor limit applies");
    assert_eq!(
        effective.sustained.rate, 5,
        "an enforce ancestor limit cannot be exceeded"
    );
    assert!(
        target.inherited,
        "the resolved upstream belongs to the ancestor"
    );
}

#[tokio::test]
async fn get_and_post_routes_coexist_on_the_same_path() {
    let fixture = fixture();
    let created = create_upstream_with_route(
        &fixture,
        &fixture.child_ctx(),
        upstream(fixture.child.0, "api.vendor.com"),
    )
    .await;

    fixture
        .control_plane()
        .create_route(
            &fixture.child_ctx(),
            route(fixture.child.0, created.id, &["POST"]),
        )
        .await
        .expect("a second route that differs only by method must be accepted");

    let target = fixture
        .control_plane()
        .resolve_proxy_target(
            &fixture.child_ctx(),
            "api.vendor.com",
            "POST",
            "/v1/items",
            "",
        )
        .await
        .expect("target");
    assert_eq!(
        target.route.match_config.http.unwrap().methods,
        vec!["POST".to_owned()],
        "the POST route is matched, not the GET one"
    );
}

#[tokio::test]
async fn binding_to_an_enforce_ancestor_alias_is_rejected() {
    let fixture = fixture_with(Vec::new());
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.rate_limit = Some(rate_limit(SharingMode::Enforce, 10));
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    let error = fixture
        .control_plane()
        .create_upstream(
            &fixture.child_ctx(),
            upstream(fixture.child.0, "api.vendor.com"),
        )
        .await
        .expect_err("an enforce ancestor's alias cannot be overridden");
    assert_eq!(error.status(), 403, "{error:?}");
    let descriptor = error.descriptor();
    assert_eq!(
        descriptor.type_id,
        "gts.cf.core.errors.err.v1~cf.oagw.forbidden.v1"
    );
    assert_eq!(descriptor.title, "Forbidden");
    assert_eq!(descriptor.error_code, "FORBIDDEN");
}

#[tokio::test]
async fn a_disabled_ancestor_alias_can_still_be_shadowed() {
    let fixture = fixture();
    let mut root_upstream = upstream(fixture.root.0, "api.vendor.com");
    root_upstream.rate_limit = Some(rate_limit(SharingMode::Enforce, 10));
    root_upstream.enabled = false;
    create_upstream_with_route(&fixture, &fixture.root_ctx(), root_upstream).await;

    create_upstream_with_route(
        &fixture,
        &fixture.child_ctx(),
        upstream(fixture.child.0, "api.vendor.com"),
    )
    .await;
    let count = fixture
        .control_plane()
        .list_upstreams(&fixture.child_ctx(), &ListQuery::default())
        .await
        .expect("list")
        .len();
    assert_eq!(
        count, 1,
        "a disabled ancestor cannot enforce anything on the descendant"
    );
}

#[tokio::test]
async fn catalog_only_plugin_references_are_rejected_at_create_time() {
    let fixture = fixture();
    let mut payload = upstream(fixture.child.0, "api.vendor.com");
    payload.plugins = Some(plugins(SharingMode::Private, vec![gts::AUTH_PLUGIN_BASIC]));

    let error = fixture
        .control_plane()
        .create_upstream(&fixture.child_ctx(), payload)
        .await
        .expect_err("a catalog-only identifier has no implementation");
    assert_eq!(error.status(), 400, "{error:?}");
    assert!(
        error.detail().contains("catalog-only"),
        "{}",
        error.detail()
    );
}

#[tokio::test]
async fn a_guard_reference_in_the_auth_slot_is_rejected_at_create_time() {
    let fixture = fixture();
    let mut payload = upstream(fixture.child.0, "api.vendor.com");
    payload.auth = Some(auth(
        SharingMode::Private,
        gts::GUARD_PLUGIN_REQUIRED_HEADERS,
    ));

    let error = fixture
        .control_plane()
        .create_upstream(&fixture.child_ctx(), payload)
        .await
        .expect_err("the auth slot only accepts auth plugins");
    assert_eq!(error.status(), 400, "{error:?}");
    assert!(error.detail().contains("auth_plugin"), "{}", error.detail());
}

#[tokio::test]
async fn allow_credentials_with_a_wildcard_origin_is_rejected_at_create_time() {
    let fixture = fixture();
    let mut payload = upstream(fixture.child.0, "api.vendor.com");
    let mut config = cors(SharingMode::Private, &["*"]);
    config.allow_credentials = true;
    payload.cors = Some(config);

    let error = fixture
        .control_plane()
        .create_upstream(&fixture.child_ctx(), payload)
        .await
        .expect_err("credentials with a wildcard origin are not expressible");
    assert_eq!(error.status(), 400, "{error:?}");
    assert!(
        error.detail().contains("allow_credentials"),
        "{}",
        error.detail()
    );

    // The route-level configuration is validated the same way.
    let created = create_upstream_with_route(
        &fixture,
        &fixture.child_ctx(),
        upstream(fixture.child.0, "other.vendor.com"),
    )
    .await;
    let mut route = common::http_route(created.id, "/v1", &["GET"]);
    route.tenant_id = fixture.child.0;
    let mut route_cors = cors(SharingMode::Private, &["*"]);
    route_cors.allow_credentials = true;
    route.cors = Some(route_cors);
    let error = fixture
        .control_plane()
        .create_route(&fixture.child_ctx(), route)
        .await
        .expect_err("a route cannot combine credentials with a wildcard origin");
    assert_eq!(error.status(), 400, "{error:?}");
}

#[tokio::test]
async fn an_unresolvable_tenant_hierarchy_is_a_503() {
    let child = TenantId(uuid::Uuid::new_v4());
    let resolver: Arc<dyn TenantResolverClient> = Arc::new(BrokenTenants);
    let control_plane = Arc::new(ControlPlaneServiceImpl::new(
        InMemoryStore::new(),
        Some(resolver),
    ));
    let ctx = context_for(child);

    let error = control_plane
        .resolve_proxy_target(&ctx, "api.vendor.com", "GET", "/v1/items", "")
        .await
        .expect_err("a truncated chain would silently drop ancestor constraints");
    assert!(
        matches!(error, DomainError::TenantResolution(_)),
        "{error:?}"
    );
    assert_eq!(error.status(), 503);
    let descriptor = error.descriptor();
    assert_eq!(
        descriptor.type_id,
        "gts.cf.core.errors.err.v1~cf.oagw.tenant.unavailable.v1"
    );
    assert_eq!(descriptor.title, "Tenant Unavailable");
    assert_eq!(descriptor.error_code, "TENANT_UNAVAILABLE");
}

#[tokio::test]
async fn a_pdp_denial_is_a_403_forbidden() {
    let fixture = fixture_with(vec!["create"]);
    let error = fixture
        .control_plane()
        .create_upstream(
            &fixture.child_ctx(),
            upstream(fixture.child.0, "api.vendor.com"),
        )
        .await
        .expect_err("the PDP denied `create`");
    assert_eq!(error.status(), 403, "{error:?}");
    let descriptor = error.descriptor();
    assert_eq!(
        descriptor.type_id,
        "gts.cf.core.errors.err.v1~cf.oagw.forbidden.v1"
    );
    assert_eq!(descriptor.title, "Forbidden");
    assert!(error.detail().contains(DENY_DETAILS), "{}", error.detail());
}
