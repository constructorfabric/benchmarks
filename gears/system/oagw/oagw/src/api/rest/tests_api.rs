//! Handler- and service-level tests for the OAGW Control-Plane Management API
//! (feature `cpt-cf-oagw-feature-control-plane-api`, flows
//! `cpt-cf-oagw-flow-control-plane-api-{upstream,route,plugin}-crud`).
//!
//! Coverage:
//! - alias derivation/enforcement rules (auto-derive, explicit-required,
//!   idempotent no-op, immutability) — algorithm
//!   `cpt-cf-oagw-algo-control-plane-api-derive-alias`;
//! - management validation: duplicate alias 409, route collision 409, plugin
//!   in-use 409, 400s — algorithms
//!   `cpt-cf-oagw-algo-control-plane-api-validate-*`;
//! - permission enforcement (403 `access.denied`, 503 on evaluation failure) —
//!   algorithm `cpt-cf-oagw-algo-control-plane-api-authorize`;
//! - OData list params ($filter/$orderby/$top/$skip) — algorithm
//!   `cpt-cf-oagw-algo-control-plane-api-odata`;
//! - the RFC 9457 envelope with the OAGW GTS instance `type` and
//!   `X-OAGW-Error-Source: gateway` header (DoD
//!   `cpt-cf-oagw-dod-error-semantics-envelope`).
//!
//! The PDP is faked: [`AllowAllAuthz`] mirrors the `static-authz-plugin`
//! behaviour (resolve tenant from the subject, constrain `owner_tenant_id` to
//! it, deny nil tenants), [`DenyAuthz`] always denies, [`FailAuthz`] fails the
//! evaluation RPC.  The tenant/types-registry/clients are never consulted on
//! the control plane, so their fakes are inert.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::header;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use types_registry_sdk::testing::MockTypesRegistryClient;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::GearState;
use crate::domain::entity::config::{EndpointScheme, HeadersConfig, SharingMode, UpstreamProtocol};
use crate::domain::entity::route::{GrpcMatch, HttpMatch, PathSuffixMode, RouteMatch, RouteMethod};
use crate::domain::entity::upstream::{AuthConfig, Endpoint, ServerConfig};
use crate::domain::error::DomainError;
use crate::domain::plugin::PluginRegistries;
use crate::domain::rate::{Decider, RateLimiter};
use crate::domain::service::DataPlaneService;
use crate::domain::service::control_plane::{ControlPlaneService, RouteDraft, UpstreamDraft};
use crate::infra::UpstreamRepoOptions;
use crate::infra::storage::InMemoryStore;

use super::routes;

use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverClient, AuthZResolverError};
use tenant_resolver_sdk::TenantResolverClient;
use toolkit_security::pep_properties;

// ---------------------------------------------------------------------------
// Test constants
// ---------------------------------------------------------------------------

fn tenant_a() -> Uuid {
    Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
}
fn tenant_b() -> Uuid {
    Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap()
}
fn subject() -> Uuid {
    Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap()
}

fn ctx(tid: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject())
        .subject_tenant_id(tid)
        .build()
        .unwrap()
}

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Minimal valid upstream request body (single hostname endpoint).
fn upstream_body(host: &str) -> Value {
    json!({
        "server": { "endpoints": [{ "scheme": "https", "host": host, "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
    })
}

fn ep(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn upstream_draft(alias: Option<&str>, endpoints: Vec<Endpoint>) -> UpstreamDraft {
    UpstreamDraft {
        alias: alias.map(str::to_owned),
        enabled: None,
        tags: Vec::new(),
        server: ServerConfig { endpoints },
        protocol: UpstreamProtocol::Http,
        auth: AuthConfig::default(),
        headers: HeadersConfig::default(),
        rate_limit: None,
        cors: None,
        plugin_refs: Vec::new(),
        plugins_sharing: SharingMode::Private,
    }
}

/// Seed a hostname-backed upstream directly in the repo (bypasses the alias
/// derivation rules exercised elsewhere).
fn seeded_upstream(host: &str) -> Upstream {
    Upstream::new(
        tenant_a(),
        host,
        ServerConfig {
            endpoints: vec![ep(EndpointScheme::Https, host, 443)],
        },
    )
}

fn http_match(methods: Vec<RouteMethod>, path: &str) -> HttpMatch {
    HttpMatch {
        methods,
        path_prefix: path.to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

fn route_draft(upstream_id: Uuid, path: &str, priority: i32) -> RouteDraft {
    RouteDraft {
        upstream_id,
        match_: RouteMatch::Http(http_match(vec![RouteMethod::Get], path)),
        priority,
        enabled: true,
        tags: Vec::new(),
        rate_limit: None,
        cors: None,
        plugin_refs: Vec::new(),
        plugins_sharing: SharingMode::Private,
    }
}

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

/// PDP mock mirroring `static-authz-plugin`: constrain `owner_tenant_id` to
/// the subject tenant; nil/absent tenant → deny.
struct AllowAllAuthz;

#[async_trait]
impl AuthZResolverClient for AllowAllAuthz {
    async fn evaluate(
        &self,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        let tid = req
            .context
            .tenant_context
            .as_ref()
            .and_then(|tc| tc.root_id)
            .or_else(|| {
                req.subject
                    .properties
                    .get("tenant_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            });
        let Some(tid) = tid else {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        };
        if tid == Uuid::default() {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        }
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [tid],
                    ))],
                }],
                ..Default::default()
            },
        })
    }
}

/// PDP mock that always denies.
struct DenyAuthz;

#[async_trait]
impl AuthZResolverClient for DenyAuthz {
    async fn evaluate(
        &self,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// PDP mock that grants everything except `read` on `transform_plugin`
/// resources (partial grants — used to exercise the list's drop semantics).
struct PartialPluginsAuthz;

#[async_trait]
impl AuthZResolverClient for PartialPluginsAuthz {
    async fn evaluate(
        &self,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        if req.resource.resource_type == "gts.cf.core.oagw.transform_plugin.v1~"
            && req.action.name == "read"
        {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        }
        let tid = req
            .context
            .tenant_context
            .as_ref()
            .and_then(|tc| tc.root_id)
            .or_else(|| {
                req.subject
                    .properties
                    .get("tenant_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            });
        let Some(tid) = tid else {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        };
        if tid == Uuid::default() {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        }
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [tid],
                    ))],
                }],
                ..Default::default()
            },
        })
    }
}

/// PDP mock that fails the evaluation RPC (→ 503).
struct FailAuthz;

#[async_trait]
impl AuthZResolverClient for FailAuthz {
    async fn evaluate(
        &self,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Err(AuthZResolverError::Internal("boom".to_owned()))
    }
}

/// Tenant resolver fake — never consulted by the control plane.
struct FakeTenantResolver;

#[async_trait]
impl TenantResolverClient for FakeTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: tenant_resolver_sdk::models::TenantId,
    ) -> Result<tenant_resolver_sdk::models::TenantInfo, tenant_resolver_sdk::TenantResolverError>
    {
        unreachable!("control plane does not call the tenant resolver")
    }
    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<tenant_resolver_sdk::models::TenantInfo, tenant_resolver_sdk::TenantResolverError>
    {
        unreachable!("control plane does not call the tenant resolver")
    }
    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[tenant_resolver_sdk::models::TenantId],
        _options: &tenant_resolver_sdk::models::GetTenantsOptions,
    ) -> Result<
        Vec<tenant_resolver_sdk::models::TenantInfo>,
        tenant_resolver_sdk::TenantResolverError,
    > {
        unreachable!("control plane does not call the tenant resolver")
    }
    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        _id: tenant_resolver_sdk::models::TenantId,
        _options: &tenant_resolver_sdk::models::GetAncestorsOptions,
    ) -> Result<
        tenant_resolver_sdk::models::GetAncestorsResponse,
        tenant_resolver_sdk::TenantResolverError,
    > {
        unreachable!("control plane does not call the tenant resolver")
    }
    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        _id: tenant_resolver_sdk::models::TenantId,
        _options: &tenant_resolver_sdk::models::GetDescendantsOptions,
    ) -> Result<
        tenant_resolver_sdk::models::GetDescendantsResponse,
        tenant_resolver_sdk::TenantResolverError,
    > {
        unreachable!("control plane does not call the tenant resolver")
    }
    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor: tenant_resolver_sdk::models::TenantId,
        _descendant: tenant_resolver_sdk::models::TenantId,
        _options: &tenant_resolver_sdk::models::IsAncestorOptions,
    ) -> Result<bool, tenant_resolver_sdk::TenantResolverError> {
        unreachable!("control plane does not call the tenant resolver")
    }
}

/// Builds control-plane dependencies over one in-memory store.
#[allow(clippy::type_complexity)] // test helper assembling a 4-tuple of trait objects
fn deps(
    authz: Arc<dyn AuthZResolverClient>,
) -> (
    ControlPlaneService,
    Arc<dyn crate::domain::repo::UpstreamRepository>,
    Arc<dyn crate::domain::repo::RouteRepository>,
    Arc<dyn crate::domain::repo::PluginRepository>,
) {
    deps_with_config(OagwConfig::default(), authz)
}

/// Like [`deps`] but with an explicit gear config.
#[allow(clippy::type_complexity)] // test helper assembling a 4-tuple of trait objects
fn deps_with_config(
    cfg: OagwConfig,
    authz: Arc<dyn AuthZResolverClient>,
) -> (
    ControlPlaneService,
    Arc<dyn crate::domain::repo::UpstreamRepository>,
    Arc<dyn crate::domain::repo::RouteRepository>,
    Arc<dyn crate::domain::repo::PluginRepository>,
) {
    let store = InMemoryStore::new();
    // Mirror gear.rs: the store's config-boundary guard shares the gear's
    // `allow_http_upstream` so the repo layer agrees with the service.
    let upstreams = store.upstream_repo(UpstreamRepoOptions {
        allow_http_upstream: cfg.allow_http_upstream,
    });
    let routes = store.route_repo();
    let plugins = store.plugin_repo();
    let control = ControlPlaneService::new(
        cfg,
        upstreams.clone(),
        routes.clone(),
        plugins.clone(),
        Arc::new(FakeTenantResolver),
        authz,
    );
    (control, upstreams, routes, plugins)
}

/// Builds a router with all control-plane operations registered over a fresh
/// store (also exercises `OpenApiRegistryImpl` — duplicate paths/operation ids
/// or schema collisions would panic here).
fn router(authz: Arc<dyn AuthZResolverClient>) -> Router {
    router_with_config(OagwConfig::default(), authz)
}

/// Like [`router`] but with an explicit gear config (e.g. enabling
/// `allow_http_upstream` so plaintext-endpoint scenarios are testable on the
/// wire — FEATURE `inst-dm-ssrf-scheme`, `inst-cp-ssrf-config-boundary`).
fn router_with_config(cfg: OagwConfig, authz: Arc<dyn AuthZResolverClient>) -> Router {
    let store = InMemoryStore::new();
    // Mirror gear.rs: the store's config-boundary guard shares the gear's
    // `allow_http_upstream` so the repo layer agrees with the service.
    let upstreams = store.upstream_repo(UpstreamRepoOptions {
        allow_http_upstream: cfg.allow_http_upstream,
    });
    let routes = store.route_repo();
    let plugins = store.plugin_repo();
    let control = ControlPlaneService::new(
        cfg.clone(),
        upstreams.clone(),
        routes.clone(),
        plugins.clone(),
        Arc::new(FakeTenantResolver),
        authz.clone(),
    );
    let metrics = Arc::new(crate::infra::MetricsRegistry::default());
    let data = DataPlaneService::new(
        cfg,
        upstreams,
        routes,
        plugins,
        PluginRegistries::new(),
        Arc::new(RateLimiter::new(Duration::from_secs(60))) as Arc<dyn Decider>,
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
        Arc::new(MockTypesRegistryClient::new()),
        Arc::new(FakeTenantResolver),
        authz,
        Arc::clone(&metrics),
    );
    let registry = toolkit::api::OpenApiRegistryImpl::new();
    routes::register_routes(
        Router::new(),
        &registry,
        Arc::new(GearState {
            control,
            data,
            metrics,
        }),
    )
}

/// Sends one request carrying the subject `SecurityContext` and returns the
/// status, parsed JSON body, and response headers.
async fn request(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    tid: Uuid,
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .extension(ctx(tid));
    if let Some(b) = &body {
        builder = builder.header(header::CONTENT_LENGTH, b.to_string().len());
    }
    let req = builder
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value, headers)
}

fn problem_type(body: &Value) -> String {
    body.get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Service-level tests (alias rules, permissions, collisions)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hostname_pool_auto_derives_alias() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let created = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(None, vec![ep(EndpointScheme::Https, "api.vendor.com", 443)]),
        )
        .await
        .expect("derivable pool should succeed");
    assert_eq!(created.alias, "api.vendor.com");
}

#[tokio::test]
async fn multi_host_pool_derives_common_suffix() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let created = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(
                None,
                vec![
                    ep(EndpointScheme::Https, "us.vendor.com", 443),
                    ep(EndpointScheme::Https, "eu.vendor.com", 443),
                ],
            ),
        )
        .await
        .expect("multi-host pool should derive");
    assert_eq!(created.alias, "vendor.com");
}

#[tokio::test]
async fn ip_pool_requires_explicit_alias() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let err = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(None, vec![ep(EndpointScheme::Https, "10.0.0.1", 443)]),
        )
        .await
        .expect_err("IP pool without explicit alias must fail");
    assert_eq!(err.status(), 400);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    // With an explicit alias it succeeds.
    let created = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(
                Some("gw-a"),
                vec![ep(EndpointScheme::Https, "10.0.0.1", 443)],
            ),
        )
        .await
        .expect("explicit alias should make the pool valid");
    assert_eq!(created.alias, "gw-a");
}

#[tokio::test]
async fn matching_user_alias_on_hostname_pool_is_idempotent() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let created = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(
                Some("api.vendor.com"),
                vec![ep(EndpointScheme::Https, "api.vendor.com", 443)],
            ),
        )
        .await
        .expect("matching alias should be an idempotent no-op");
    assert_eq!(created.alias, "api.vendor.com");
}

#[tokio::test]
async fn non_matching_user_alias_on_hostname_pool_is_rejected() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let err = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(
                Some("my-alias"),
                vec![ep(EndpointScheme::Https, "api.vendor.com", 443)],
            ),
        )
        .await
        .expect_err("non-matching alias must fail");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn duplicate_alias_is_409_alias_conflict() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    svc.create_upstream(
        &ctx(tenant_a()),
        upstream_draft(
            Some("gw-a"),
            vec![ep(EndpointScheme::Https, "10.0.0.1", 443)],
        ),
    )
    .await
    .expect("first create succeeds");
    let err = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(
                Some("gw-a"),
                vec![ep(EndpointScheme::Https, "10.0.0.2", 443)],
            ),
        )
        .await
        .expect_err("duplicate alias must fail");
    assert_eq!(err.status(), 409);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1"
    );
}

#[tokio::test]
async fn replace_with_changed_alias_is_400_immutability() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let created = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(
                Some("gw-a"),
                vec![ep(EndpointScheme::Https, "10.0.0.1", 443)],
            ),
        )
        .await
        .expect("create succeeds");
    let err = svc
        .replace_upstream(
            &ctx(tenant_a()),
            created.id,
            upstream_draft(
                Some("gw-b"),
                vec![ep(EndpointScheme::Https, "10.0.0.1", 443)],
            ),
        )
        .await
        .expect_err("alias change must be refused");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn replace_with_same_alias_succeeds() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let created = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(
                Some("api.vendor.com"),
                vec![ep(EndpointScheme::Https, "api.vendor.com", 443)],
            ),
        )
        .await
        .expect("create succeeds");
    let replaced = svc
        .replace_upstream(
            &ctx(tenant_a()),
            created.id,
            upstream_draft(
                Some("api.vendor.com"),
                vec![ep(EndpointScheme::Https, "api.vendor.com", 443)],
            ),
        )
        .await
        .expect("same alias replacement succeeds");
    assert_eq!(replaced.unwrap().id, created.id);
}

#[tokio::test]
async fn denied_action_is_403_access_denied() {
    let (svc, _, _, _) = deps(Arc::new(DenyAuthz));
    let err = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(None, vec![ep(EndpointScheme::Https, "api.vendor.com", 443)]),
        )
        .await
        .expect_err("denied create must fail");
    assert_eq!(err.status(), 403);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.access.denied.v1"
    );
}

#[tokio::test]
async fn evaluation_failure_is_503() {
    let (svc, _, _, _) = deps(Arc::new(FailAuthz));
    let err = svc
        .list_upstreams(&ctx(tenant_a()))
        .await
        .expect_err("evaluation failure must fail closed");
    assert_eq!(err.status(), 503);
    assert!(matches!(err, DomainError::ServiceUnavailable { .. }));
}

#[tokio::test]
async fn grpc_route_is_rejected_400() {
    let (svc, upstreams, _, _) = deps(Arc::new(AllowAllAuthz));
    let up = upstreams
        .create(
            tenant_a(),
            Upstream::new(
                tenant_a(),
                "api.vendor.com",
                ServerConfig {
                    endpoints: vec![ep(EndpointScheme::Https, "api.vendor.com", 443)],
                },
            ),
        )
        .await
        .expect("seed upstream");
    let mut draft = route_draft(up.id, "/v1", 0);
    draft.match_ = RouteMatch::Grpc(GrpcMatch {
        service: "foo.v1.UserService".to_owned(),
        method: "GetUser".to_owned(),
    });
    let err = svc
        .create_route(&ctx(tenant_a()), draft)
        .await
        .expect_err("grpc routes are reserved");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn route_to_missing_upstream_is_400() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let err = svc
        .create_route(&ctx(tenant_a()), route_draft(Uuid::new_v4(), "/v1", 0))
        .await
        .expect_err("missing upstream must fail");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn route_collision_is_409() {
    let (svc, upstreams, _, _) = deps(Arc::new(AllowAllAuthz));
    let up = upstreams
        .create(tenant_a(), seeded_upstream("api.vendor.com"))
        .await
        .expect("seed upstream");
    svc.create_route(&ctx(tenant_a()), route_draft(up.id, "/v1", 10))
        .await
        .expect("first route succeeds");
    let err = svc
        .create_route(&ctx(tenant_a()), route_draft(up.id, "/v1", 10))
        .await
        .expect_err("identical (path, priority) on same upstream must conflict");
    assert_eq!(err.status(), 409);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.route.conflict.v1"
    );
}

#[tokio::test]
async fn same_path_different_priority_is_not_a_collision() {
    let (svc, upstreams, _, _) = deps(Arc::new(AllowAllAuthz));
    let up = upstreams
        .create(tenant_a(), seeded_upstream("api.vendor.com"))
        .await
        .expect("seed upstream");
    svc.create_route(&ctx(tenant_a()), route_draft(up.id, "/v1", 10))
        .await
        .expect("first route succeeds");
    svc.create_route(&ctx(tenant_a()), route_draft(up.id, "/v1", 20))
        .await
        .expect("different priority is a distinct route");
}

#[tokio::test]
async fn replace_route_with_different_upstream_is_400() {
    let (svc, upstreams, _, _) = deps(Arc::new(AllowAllAuthz));
    let up_a = upstreams
        .create(tenant_a(), seeded_upstream("a.vendor.com"))
        .await
        .unwrap();
    let up_b = upstreams
        .create(tenant_a(), seeded_upstream("b.vendor.com"))
        .await
        .unwrap();
    let route = svc
        .create_route(&ctx(tenant_a()), route_draft(up_a.id, "/v1", 0))
        .await
        .expect("create succeeds");
    let err = svc
        .replace_route(&ctx(tenant_a()), route.id, route_draft(up_b.id, "/v1", 0))
        .await
        .expect_err("upstream_id is immutable");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn plugin_duplicate_name_is_409() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    svc.create_plugin(
        &ctx(tenant_a()),
        crate::domain::entity::PluginType::Auth,
        "apikey".to_owned(),
        json!({}),
        "def auth(ctx): pass".to_owned(),
    )
    .await
    .expect("first create succeeds");
    let err = svc
        .create_plugin(
            &ctx(tenant_a()),
            crate::domain::entity::PluginType::Auth,
            "apikey".to_owned(),
            json!({}),
            "def auth(ctx): pass".to_owned(),
        )
        .await
        .expect_err("duplicate (tenant, name) must conflict");
    assert_eq!(err.status(), 409);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.conflict.v1"
    );
}

#[tokio::test]
async fn plugin_in_use_delete_is_409_with_referenced_by() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let plugin = svc
        .create_plugin(
            &ctx(tenant_a()),
            crate::domain::entity::PluginType::Auth,
            "apikey".to_owned(),
            json!({}),
            "def auth(ctx): pass".to_owned(),
        )
        .await
        .expect("create plugin");

    let mut draft = upstream_draft(None, vec![ep(EndpointScheme::Https, "api.vendor.com", 443)]);
    draft.plugin_refs = vec![plugin.id.to_string()];
    let up = svc
        .create_upstream(&ctx(tenant_a()), draft)
        .await
        .expect("bind plugin to upstream");

    // Unbound delete succeeds.
    let plugin2 = svc
        .create_plugin(
            &ctx(tenant_a()),
            crate::domain::entity::PluginType::Guard,
            "noop-guard".to_owned(),
            json!({}),
            "def guard(ctx): return True".to_owned(),
        )
        .await
        .unwrap();
    assert!(
        svc.delete_plugin(&ctx(tenant_a()), plugin2.id)
            .await
            .expect("unbound delete succeeds")
    );

    // Bound delete is refused with referenced_by.
    let err = svc
        .delete_plugin(&ctx(tenant_a()), plugin.id)
        .await
        .expect_err("bound plugin must be refused");
    assert_eq!(err.status(), 409);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    let refs = err.referenced_by();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].id, up.id);
}

#[tokio::test]
async fn resources_are_tenant_scoped() {
    let (svc, _, _, _) = deps(Arc::new(AllowAllAuthz));
    let created = svc
        .create_upstream(
            &ctx(tenant_a()),
            upstream_draft(None, vec![ep(EndpointScheme::Https, "api.vendor.com", 443)]),
        )
        .await
        .expect("create in tenant A");
    // Tenant B never sees tenant A's upstream.
    assert!(
        svc.get_upstream(&ctx(tenant_b()), created.id)
            .await
            .expect("read from tenant B")
            .is_none()
    );
    assert!(
        !svc.delete_upstream(&ctx(tenant_b()), created.id)
            .await
            .expect("delete from tenant B")
    );
}

// ---------------------------------------------------------------------------
// Handler-level tests (wire envelope + OData)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_upstream_returns_201_location_and_view() {
    let router = router(Arc::new(AllowAllAuthz));
    let (status, body, headers) = request(
        router,
        "POST",
        "/upstreams",
        Some(upstream_body("api.vendor.com")),
        tenant_a(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let location = headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        location,
        format!("/upstreams/{}", body["id"].as_str().unwrap())
    );
    assert_eq!(body["alias"], "api.vendor.com");
    assert_eq!(body["enabled"], true);
}

#[tokio::test]
async fn duplicate_alias_is_409_problem_json_with_gts_instance() {
    let router = router(Arc::new(AllowAllAuthz));
    let body = json!({
        "alias": "gw-a",
        "server": { "endpoints": [{ "scheme": "https", "host": "10.0.0.1", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
    });
    let router2 = router.clone();
    let (s1, _, _) = request(router, "POST", "/upstreams", Some(body.clone()), tenant_a()).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, body2, headers) = request(router2, "POST", "/upstreams", Some(body), tenant_a()).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(
        problem_type(&body2),
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1"
    );
    assert_eq!(body2["status"], 409);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert!(body2["detail"].as_str().is_some_and(|d| d.contains("gw-a")));
}

#[tokio::test]
async fn ip_pool_without_alias_is_400_validation() {
    let router = router(Arc::new(AllowAllAuthz));
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "10.0.0.1", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
    });
    let (status, body, _) = request(router, "POST", "/upstreams", Some(body), tenant_a()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn bad_alias_is_400_without_touching_store() {
    let router = router(Arc::new(AllowAllAuthz));
    let body = json!({
        "alias": "under_score",
        "server": { "endpoints": [{ "scheme": "https", "host": "10.0.0.1", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
    });
    // Alias normalization lowercases and strips trailing dots, but `_` is
    // outside `[a-z0-9.:-]`, so the service's validate_alias rejects it.
    let (status, body, _) = request(router, "POST", "/upstreams", Some(body), tenant_a()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn plaintext_http_scheme_rejected_at_boundary_when_disabled() {
    // FEATURE `inst-dm-ssrf-scheme`: plaintext `http` is not in the default
    // allowlist — the configuration write is rejected and nothing persists.
    let router = router(Arc::new(AllowAllAuthz));
    let body = json!({
        "server": { "endpoints": [{ "scheme": "http", "host": "api.vendor.com", "port": 80 }] },
        "protocol": HTTP_PROTOCOL,
    });
    let (status, body, headers) =
        request(router, "POST", "/upstreams", Some(body), tenant_a()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|d| d.contains("not allow-listed")),
        "detail names the scheme allowlist rejection: {body}"
    );
}

#[tokio::test]
async fn plaintext_http_scheme_round_trips_when_allow_http_enabled() {
    // FEATURE `inst-dm-ssrf-scheme`: with `allow_http_upstream` enabled the
    // plaintext pool is accepted and `scheme` round-trips on the wire.
    let cfg = OagwConfig {
        allow_http_upstream: true,
        ..Default::default()
    };
    let router = router_with_config(cfg.clone(), Arc::new(AllowAllAuthz));
    // An IP-based pool requires an explicit alias (no derivation), keeping
    // this test focused on the scheme itself.
    let body = json!({
        "alias": "plain-vendor",
        "server": { "endpoints": [{ "scheme": "http", "host": "10.0.0.1", "port": 80 }] },
        "protocol": HTTP_PROTOCOL,
    });
    let (status, body, _) = request(router, "POST", "/upstreams", Some(body), tenant_a()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["alias"], "plain-vendor");
    assert_eq!(body["server"]["endpoints"][0]["scheme"], "http");
    assert_eq!(body["server"]["endpoints"][0]["port"], 80);
}

#[tokio::test]
async fn route_create_and_conflict_409() {
    let router = router(Arc::new(AllowAllAuthz));
    let (s1, up, _) = request(
        router.clone(),
        "POST",
        "/upstreams",
        Some(upstream_body("api.vendor.com")),
        tenant_a(),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);
    let upstream_id = up["id"].as_str().unwrap();

    let route = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "priority": 10,
    });
    let (s2, _, _) = request(
        router.clone(),
        "POST",
        "/routes",
        Some(route.clone()),
        tenant_a(),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);

    let (s3, body, _) = request(router, "POST", "/routes", Some(route), tenant_a()).await;
    assert_eq!(s3, StatusCode::CONFLICT);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.route.conflict.v1"
    );
}

#[tokio::test]
async fn route_to_missing_upstream_is_400_handler() {
    let router = router(Arc::new(AllowAllAuthz));
    let route = json!({
        "upstream_id": Uuid::new_v4().to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
    });
    let (status, body, _) = request(router, "POST", "/routes", Some(route), tenant_a()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn denied_create_is_403_access_denied_envelope() {
    let router = router(Arc::new(DenyAuthz));
    let (status, body, headers) = request(
        router,
        "POST",
        "/upstreams",
        Some(upstream_body("api.vendor.com")),
        tenant_a(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.access.denied.v1"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn missing_upstream_get_is_404_route_not_found() {
    let router = router(Arc::new(AllowAllAuthz));
    let (status, body, _) = request(
        router,
        "GET",
        &format!("/upstreams/{}", Uuid::new_v4()),
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn plugin_in_use_delete_is_409_with_referenced_by_body() {
    let router = router(Arc::new(AllowAllAuthz));
    let plugin = json!({
        "plugin_type": "auth",
        "name": "apikey",
        "config_schema": {},
        "source_code": "def auth(ctx): pass",
    });
    let (s1, created, _) =
        request(router.clone(), "POST", "/plugins", Some(plugin), tenant_a()).await;
    assert_eq!(s1, StatusCode::CREATED);
    let plugin_id = created["id"].as_str().unwrap();

    let up = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "sharing": "private", "items": [plugin_id] },
    });
    let (s2, _, _) = request(router.clone(), "POST", "/upstreams", Some(up), tenant_a()).await;
    assert_eq!(s2, StatusCode::CREATED);

    let (s3, body, _) = request(
        router,
        "DELETE",
        &format!("/plugins/{plugin_id}"),
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s3, StatusCode::CONFLICT);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    let refs = body["referenced_by"]
        .as_array()
        .expect("referenced_by present");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0]["resource"], "upstream");
}

#[tokio::test]
async fn list_applies_filter_order_and_paging() {
    let router = router(Arc::new(AllowAllAuthz));
    // Three IP-backed upstreams with explicit aliases keep the pool derivability
    // out of the way.
    for alias in ["a", "b", "c"] {
        let body = json!({
            "alias": alias,
            "server": { "endpoints": [{ "scheme": "https", "host": "10.0.0.1", "port": 443 }] },
            "protocol": HTTP_PROTOCOL,
        });
        let (s, _, _) = request(router.clone(), "POST", "/upstreams", Some(body), tenant_a()).await;
        assert_eq!(s, StatusCode::CREATED);
    }

    // $filter = alias eq 'b'
    let (s, body, _) = request(
        router.clone(),
        "GET",
        "/upstreams?$filter=alias%20eq%20'b'",
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["alias"], "b");

    // $orderby=alias desc + $top=2
    let (s, body, _) = request(
        router.clone(),
        "GET",
        "/upstreams?$orderby=alias%20desc&$top=2",
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["alias"], "c");
    assert_eq!(items[1]["alias"], "b");

    // $skip=1 (with an explicit order to stay deterministic — the in-memory
    // store orders rows by random UUID) → b, c
    let (s, body, _) = request(
        router,
        "GET",
        "/upstreams?$orderby=alias%20asc&$skip=1",
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let aliases: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["alias"].as_str().unwrap())
        .collect();
    assert_eq!(aliases, vec!["b", "c"]);
}

#[tokio::test]
async fn list_with_unknown_orderby_key_is_400() {
    let router = router(Arc::new(AllowAllAuthz));
    let (status, body, _) =
        request(router, "GET", "/upstreams?$orderby=bogus", None, tenant_a()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn plugin_source_endpoint_returns_source() {
    let router = router(Arc::new(AllowAllAuthz));
    let plugin = json!({
        "plugin_type": "auth",
        "name": "apikey",
        "config_schema": {},
        "source_code": "def auth(ctx): pass",
    });
    let (s1, created, _) =
        request(router.clone(), "POST", "/plugins", Some(plugin), tenant_a()).await;
    assert_eq!(s1, StatusCode::CREATED);
    let plugin_id = created["id"].as_str().unwrap();

    let (s2, body, _) = request(
        router,
        "GET",
        &format!("/plugins/{plugin_id}/source"),
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(body["source_code"], "def auth(ctx): pass");
}

#[tokio::test]
async fn list_plugins_skips_types_the_caller_cannot_read() {
    // F-008: the list documents "drops any plugin outside the granted scope".
    // A caller lacking `read` on one plugin type must still get the plugins it
    // *can* read — the denial skips that type instead of failing the whole
    // list with 403.
    let router = router(Arc::new(PartialPluginsAuthz));
    let guard = json!({
        "plugin_type": "guard",
        "name": "req_headers",
        "config_schema": {},
        "source_code": "def guard(ctx): pass",
    });
    let (s1, _created_guard, _) =
        request(router.clone(), "POST", "/plugins", Some(guard), tenant_a()).await;
    assert_eq!(s1, StatusCode::CREATED, "guard plugin created");

    let transform = json!({
        "plugin_type": "transform",
        "name": "request_id",
        "config_schema": {},
        "source_code": "def transform(ctx): pass",
    });
    let (s2, _created_transform, _) = request(
        router.clone(),
        "POST",
        "/plugins",
        Some(transform),
        tenant_a(),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED, "transform plugin created");

    let (s3, body, _headers) = request(router, "GET", "/plugins", None, tenant_a()).await;
    assert_eq!(s3, StatusCode::OK, "list succeeds despite a denied type");
    let items = body.as_array().expect("plugin list is an array");
    assert_eq!(
        items.len(),
        1,
        "denied transform plugin dropped, guard kept"
    );
    assert_eq!(items[0]["plugin_type"], "guard");
}

// ---------------------------------------------------------------------------
// Feature acceptance-gap closers (FEATURE §13 / §20)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_13_plaintext_scheme_write_rejected_at_config_boundary_and_nothing_persisted() {
    // FEATURE §13: "A configuration write for an upstream with a
    // non-allowlisted scheme (e.g., plaintext http while
    // allow_http_upstream=false) is rejected at the configuration boundary
    // and nothing is persisted."
    //
    // REST surface: the `scheme` DTO enum admits plaintext `http`, so the
    // write deserializes and reaches the configuration-boundary SSRF guard,
    // which rejects it (400) while `allow_http_upstream` is false — and
    // nothing is persisted.
    let router = router(Arc::new(AllowAllAuthz));
    let body = json!({
        "server": { "endpoints": [{ "scheme": "http", "host": "api.vendor.com", "port": 80 }] },
        "protocol": HTTP_PROTOCOL,
    });
    let (status, resp, _headers) =
        request(router.clone(), "POST", "/upstreams", Some(body), tenant_a()).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "plaintext write rejected at the config boundary (scheme not allow-listed)"
    );
    assert_eq!(
        problem_type(&resp),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );

    // Nothing was persisted: the tenant list stays empty.
    let (s, listed, _) = request(router, "GET", "/upstreams", None, tenant_a()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 0, "nothing persisted");

    // Domain boundary: even a directly-constructed draft with a plaintext
    // endpoint is rejected by the configuration-boundary SSRF guard when
    // `allow_http_upstream` is false (service + repo guards), and nothing is
    // written to the repository.
    let (svc, upstreams, _, _) = deps(Arc::new(AllowAllAuthz));
    let plaintext = upstream_draft(
        Some("gw-http"),
        vec![ep(EndpointScheme::Http, "api.vendor.com", 80)],
    );
    let err = svc
        .create_upstream(&ctx(tenant_a()), plaintext)
        .await
        .expect_err("plaintext endpoint rejected at the boundary");
    assert_eq!(err.status(), 400);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        err.to_string().contains("allow_http_upstream"),
        "guard names the policy"
    );
    assert_eq!(
        upstreams.list(tenant_a()).await.len(),
        0,
        "rejected write leaves the repository untouched"
    );
}

#[tokio::test]
async fn ac_20_cors_credentials_with_wildcard_origin_rejected_as_400() {
    // FEATURE §20 "Upstream creation carrying CORS config with
    // allow_credentials: true and the wildcard origin is rejected at
    // validation time with 400"; FEATURE §48 "Upstream/route configuration
    // combining allow_credentials: true with the wildcard origin is rejected
    // at validation time."
    let router = router(Arc::new(AllowAllAuthz));

    // Upstream creation.
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
        "cors": {
            "enabled": true,
            "allowed_origins": ["*"],
            "allowed_methods": ["GET"],
            "allow_credentials": true
        },
    });
    let (status, resp, headers) =
        request(router.clone(), "POST", "/upstreams", Some(body), tenant_a()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upstream CORS 400");
    assert_eq!(
        problem_type(&resp),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert!(
        resp["detail"]
            .as_str()
            .is_some_and(|d| d.contains("allow_credentials"))
    );

    // Route creation with the same conflict is also rejected at validation.
    let (s, up, _) = request(
        router.clone(),
        "POST",
        "/upstreams",
        Some(upstream_body("api.vendor.com")),
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let upstream_id = up["id"].as_str().unwrap();
    let route = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "cors": {
            "enabled": true,
            "allowed_origins": ["*"],
            "allow_credentials": true
        },
    });
    let (s, resp, _) = request(router, "POST", "/routes", Some(route), tenant_a()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "route CORS 400");
    assert_eq!(
        problem_type(&resp),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );

    // The accepted twin — credentials with a specific origin — persists.
    let ok_body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET"],
            "allow_credentials": true
        },
    });
    let (s, _, _) = request(
        router_raw(),
        "POST",
        "/upstreams",
        Some(ok_body),
        tenant_a(),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CREATED,
        "credentials + specific origin is valid"
    );
}

/// A fresh router for the same assertions in the preceding test, built like
/// [`router`] (the helper's `router` argument is consumed above).
fn router_raw() -> axum::Router {
    router(Arc::new(AllowAllAuthz))
}

#[tokio::test]
async fn ac_20_list_top_defaults_to_50_and_caps_at_100() {
    // FEATURE §20: "List endpoints honor $top (capped at 100, default 50)
    // and $skip."
    let router = router(Arc::new(AllowAllAuthz));
    // 105 IP-backed upstreams with explicit aliases.
    for i in 0..105 {
        let body = json!({
            "alias": format!("u{i:03}"),
            "server": { "endpoints": [{ "scheme": "https", "host": "10.0.0.1", "port": 443 }] },
            "protocol": HTTP_PROTOCOL,
        });
        let (s, _, _) = request(router.clone(), "POST", "/upstreams", Some(body), tenant_a()).await;
        assert_eq!(s, StatusCode::CREATED);
    }

    // No $top → the default of 50.
    let (s, body, _) = request(router.clone(), "GET", "/upstreams", None, tenant_a()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 50, "default top is 50");

    // $top beyond the cap is clamped to 100.
    let (s, body, _) = request(
        router.clone(),
        "GET",
        "/upstreams?$top=1000",
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 100, "top capped at 100");

    // $top = 100 exactly is honored, and $skip composes with the cap.
    let (s, body, _) = request(
        router.clone(),
        "GET",
        "/upstreams?$top=100",
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 100);

    let (s, body, _) = request(
        router,
        "GET",
        "/upstreams?$top=1000&$skip=50",
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body.as_array().unwrap().len(),
        55,
        "remaining 55 of 105 after skip 50"
    );
}

#[tokio::test]
async fn ac_20_ancestor_owned_upstream_route_plugin_reads_puts_deletes_are_404() {
    // FEATURE §20: "GET/PUT/DELETE on the id of an ancestor-owned upstream,
    // route, or plugin returns 404" — ancestor resources stay invisible
    // through the management surface (tenant-scoped visibility).
    let router = router(Arc::new(AllowAllAuthz));

    // Tenant A: one absolute upstream, one route, one custom plugin.
    let (s, up, _) = request(
        router.clone(),
        "POST",
        "/upstreams",
        Some(upstream_body("api.vendor.com")),
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let upstream_id = up["id"].as_str().unwrap().to_owned();

    let route = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "priority": 10,
    });
    let (s, created_route, _) =
        request(router.clone(), "POST", "/routes", Some(route), tenant_a()).await;
    assert_eq!(s, StatusCode::CREATED);
    let route_id = created_route["id"].as_str().unwrap().to_owned();

    let plugin = json!({
        "plugin_type": "auth",
        "name": "apikey",
        "config_schema": {},
        "source_code": "def auth(ctx): pass",
    });
    let (s, created_plugin, _) =
        request(router.clone(), "POST", "/plugins", Some(plugin), tenant_a()).await;
    assert_eq!(s, StatusCode::CREATED);
    let plugin_id = created_plugin["id"].as_str().unwrap().to_owned();

    // Tenant B (a sibling, never an ancestor owner) sees all three as 404.
    for (verb, uri) in [
        ("GET", format!("/upstreams/{upstream_id}")),
        ("PUT", format!("/upstreams/{upstream_id}")),
        ("DELETE", format!("/upstreams/{upstream_id}")),
        ("GET", format!("/routes/{route_id}")),
        ("PUT", format!("/routes/{route_id}")),
        ("DELETE", format!("/routes/{route_id}")),
        ("GET", format!("/plugins/{plugin_id}")),
        ("DELETE", format!("/plugins/{plugin_id}")),
    ] {
        let body = match verb {
            "PUT" if uri.starts_with("/upstreams/") => Some(upstream_body("api.vendor.com")),
            "PUT" => Some(json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } },
                "priority": 10,
            })),
            _ => None,
        };
        let (status, resp, headers) = request(router.clone(), verb, &uri, body, tenant_b()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{verb} {uri}");
        assert_eq!(
            problem_type(&resp),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            "{verb} {uri}"
        );
        assert_eq!(
            headers
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway"),
            "{verb} {uri}"
        );
    }

    // Tenant A still owns them: nothing was deleted by tenant B's 404s.
    let (s, _, _) = request(
        router.clone(),
        "GET",
        &format!("/upstreams/{upstream_id}"),
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = request(
        router,
        "GET",
        &format!("/routes/{route_id}"),
        None,
        tenant_a(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

// Re-exported entity used by the service-level tests above.
use crate::domain::entity::Upstream;
