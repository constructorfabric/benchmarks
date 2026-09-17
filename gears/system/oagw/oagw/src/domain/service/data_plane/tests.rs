//! Data-Plane Proxy tests (feature `cpt-cf-oagw-feature-data-plane-proxy`,
//! p5; flow `cpt-cf-oagw-flow-data-plane-proxy-execute`).
//!
//! Coverage:
//! - alias resolution / hierarchy shadowing + ancestor-disabled dominance
//!   (algorithm `cpt-cf-oagw-algo-data-plane-proxy-resolve-alias`);
//! - route matching: empty-allowlist rejection, method allowlist, longest
//!   path prefix, priority tiebreak (algorithm
//!   `cpt-cf-oagw-algo-data-plane-proxy-match-route`);
//! - the `X-OAGW-Target-Host` matrix (ADR 0001): single/multi pools, explicit
//!   vs common-suffix aliases, header format validation (algorithm
//!   `cpt-cf-oagw-algo-data-plane-proxy-target-host`);
//! - framing/body guards (CL+TE, invalid CL, the 100 MB cap) and query /
//!   path-suffix policy (algorithm `cpt-cf-oagw-algo-data-plane-proxy-apply-config`);
//! - effective rate limiting (min across the hierarchy) and the 429 envelope;
//! - CORS actual-request enforcement and the preflight 204 (ADR 0004);
//! - permission enforcement on `gts.cf.core.oagw.proxy.v1~:invoke`
//!   (403 `access.denied`, 503 on evaluation failure);
//! - hyper forwarding/passthrough over a real loopback upstream: host
//!   rewrite, apikey credential injection, streaming passthrough tagged
//!   `x-oagw-error-source: upstream`, upstream 4xx/5xx untouched (ADR 0007),
//!   the `proxy_timeout_secs` 504, and connect-refused 503;
//! - observability (feature `cpt-cf-oagw-feature-observability-audit`): the
//!   DESIGN §4.2 metrics counter/gauge movement across 200/404/429 outcomes,
//!   the rate-limit exceedance + usage series, routing selection series
//!   (`explicit_header`/`default`), upstream availability and error-source
//!   attribution (algorithm
//!   `cpt-cf-oagw-algo-observability-audit-record-request`); end-to-end
//!   request-id correlation (flow `cpt-cf-oagw-flow-observability-audit-correlate`,
//!   DoD `cpt-cf-oagw-dod-observability-audit-correlation`); and the audit-log
//!   entries emitted on a proxy hit and a control-plane mutation with the
//!   DESIGN §4.3 field set (DoD
//!   `cpt-cf-oagw-dod-observability-audit-audit-log`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverClient, AuthZResolverError};
use axum::Router;
use axum::body::Body;
use axum::http::header;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tenant_resolver_sdk::models::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantStatus,
};
use tenant_resolver_sdk::{TenantResolverClient, TenantResolverError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use toolkit_security::{SecurityContext, pep_properties};
use tower::ServiceExt;
use types_registry_sdk::testing::MockTypesRegistryClient;
use uuid::Uuid;

use super::*;
use crate::config::OagwConfig;
use crate::domain::GearState;
use crate::domain::entity::config::{
    BurstConfig, CorsConfig, EndpointScheme, HeadersConfig, PassthroughMode, RateLimitConfig,
    RateLimitWindow, RequestHeadersConfig, ResponseHeadersConfig, SharingMode, SustainedRate,
    UpstreamProtocol,
};
use crate::domain::entity::route::{HttpMatch, MatchType, PathSuffixMode, RouteMethod};
use crate::domain::entity::upstream::{AuthConfig, Endpoint, ServerConfig, Upstream};
use crate::domain::plugin::Headers;
use crate::domain::plugin::ids::APIKEY_AUTH;
use crate::domain::rate::RateLimiter;
use crate::domain::service::control_plane::{ControlPlaneService, UpstreamDraft};
use crate::infra::UpstreamRepoOptions;
use crate::infra::plugin::builtin_registries_with_cache;
use crate::infra::storage::InMemoryStore;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tenant_id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
/// The subject tenant (owns the seeded upstreams).
fn subject() -> Uuid {
    tenant_id(0x1111_1111_1111_1111)
}
/// A parent tenant in the hierarchy.
fn parent() -> Uuid {
    tenant_id(0x2222_2222_2222_2222)
}
/// The root tenant of the hierarchy.
fn root() -> Uuid {
    tenant_id(0x3333_3333_3333_3333)
}

fn ctx(tid: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(tenant_id(0xaaaa_aaaa_aaaa_aaaa))
        .subject_tenant_id(tid)
        .build()
        .expect("security context builds")
}

/// A configurable tenant-hierarchy resolver: `get_ancestors` returns the
/// stored direct-parent → root chain for the requested tenant.
#[derive(Clone, Default)]
struct HierarchyResolver {
    ancestors_of: HashMap<Uuid, Vec<Uuid>>,
}

impl HierarchyResolver {
    fn chain(mut self, tenant: Uuid, ancestors: &[Uuid]) -> Self {
        self.ancestors_of.insert(tenant, ancestors.to_vec());
        self
    }
}

fn tenant_ref(id: Uuid) -> TenantRef {
    TenantRef {
        id: TenantId(id),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

#[async_trait]
impl TenantResolverClient for HierarchyResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        let parent_id = self
            .ancestors_of
            .get(&id.0)
            .and_then(|chain| chain.first())
            .copied();
        Ok(TenantInfo {
            id,
            name: id.0.to_string(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: parent_id.map(TenantId),
            self_managed: false,
        })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(TenantInfo {
            id: TenantId(root()),
            name: "root".to_owned(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        })
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(Vec::new())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        let ancestors = self
            .ancestors_of
            .get(&id.0)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(tenant_ref)
            .collect();
        Ok(GetAncestorsResponse {
            tenant: tenant_ref(id.0),
            ancestors,
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Ok(GetDescendantsResponse {
            tenant: tenant_ref(id.0),
            descendants: Vec::new(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor: TenantId,
        _descendant: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Ok(false)
    }
}

/// PDP mock mirroring `static-authz-plugin` (constrain `owner_tenant_id` to
/// the subject tenant; deny nil tenants).
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

/// A data-plane fixture over one in-memory store: shared repositories, a
/// CredStore-backed built-in registry, the tenant hierarchy resolver, and
/// the shared metrics registry.
struct Fixture {
    cfg: OagwConfig,
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    credstore: Arc<dyn CredStoreClientV1>,
    rate_limiter: Arc<dyn Decider>,
    resolver: Arc<dyn TenantResolverClient>,
    metrics: Arc<MetricsRegistry>,
}

fn test_cfg(allow_http: bool, timeout_secs: u64) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: timeout_secs,
        allow_http_upstream: allow_http,
        ssrf_policy: Default::default(),
        token_cache_ttl_secs: 60,
        token_cache_capacity: 100,
    }
}

impl Fixture {
    /// Builds a fixture with `resolver` as the tenant hierarchy and an empty
    /// CredStore (apikey tests inject secrets via `with_credstore`).
    fn with_resolver(cfg: OagwConfig, resolver: Arc<dyn TenantResolverClient>) -> Self {
        let store = InMemoryStore::new();
        let upstreams = store.upstream_repo(UpstreamRepoOptions {
            allow_http_upstream: cfg.allow_http_upstream,
        });
        let routes = store.route_repo();
        let plugins = store.plugin_repo();
        Self {
            cfg,
            upstreams,
            routes,
            plugins,
            credstore: Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
            rate_limiter: Arc::new(RateLimiter::new(Duration::from_secs(60))),
            resolver,
            metrics: Arc::new(MetricsRegistry::default()),
        }
    }

    /// Like [`Fixture::with_resolver`] but with CredStore secrets populated
    /// (built-in auth plugins that resolve `cred://` refs).
    fn with_credstore(
        cfg: OagwConfig,
        resolver: Arc<dyn TenantResolverClient>,
        creds: Vec<(String, String)>,
    ) -> Self {
        let mut fixture = Self::with_resolver(cfg, resolver);
        fixture.credstore = Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
            creds,
        ));
        fixture
    }

    /// A DataPlaneService instance sharing this fixture's repositories
    /// (fresh hyper client / registries per instance).
    fn data_service(&self, authz: Arc<dyn AuthZResolverClient>) -> DataPlaneService {
        DataPlaneService::new(
            self.cfg.clone(),
            self.upstreams.clone(),
            self.routes.clone(),
            self.plugins.clone(),
            builtin_registries_with_cache(
                Some(self.credstore.clone()),
                Duration::from_secs(60),
                100,
            ),
            self.rate_limiter.clone(),
            self.credstore.clone(),
            Arc::new(MockTypesRegistryClient::new()),
            self.resolver.clone(),
            authz,
            Arc::clone(&self.metrics),
        )
    }

    /// A full HTTP router with all control-plane + proxy surfaces registered
    /// over this fixture's repositories and the given PDP.
    fn router(&self, authz: Arc<dyn AuthZResolverClient>) -> Router {
        let control = ControlPlaneService::new(
            self.cfg.clone(),
            self.upstreams.clone(),
            self.routes.clone(),
            self.plugins.clone(),
            self.resolver.clone(),
            authz.clone(),
        );
        let data = self.data_service(authz);
        let registry = toolkit::api::OpenApiRegistryImpl::new();
        crate::api::rest::register_routes(
            Router::new(),
            &registry,
            Arc::new(GearState {
                control,
                data,
                metrics: Arc::clone(&self.metrics),
            }),
        )
        .expect("proxy + control-plane routes register")
    }

    /// Seeds an upstream for `tid` via the control plane (alias + SSRF rules
    /// enforced exactly like the real API), then applies `tweak`.
    ///
    /// Note: hostname pools auto-derive their alias, so tests that need a
    /// non-derived alias (`svc`, `vendor.com`, ...) must use IP endpoints
    /// (non-derivable → explicit alias required and accepted as-is).
    async fn seed_upstream(
        &self,
        tid: Uuid,
        alias: &str,
        endpoints: Vec<Endpoint>,
        tweak: impl FnOnce(&mut UpstreamDraft),
    ) -> Upstream {
        let mut draft = UpstreamDraft {
            alias: Some(alias.to_owned()),
            enabled: None,
            tags: Vec::new(),
            server: ServerConfig { endpoints },
            protocol: UpstreamProtocol::Http,
            auth: AuthConfig::default(),
            headers: HeadersConfig {
                request: RequestHeadersConfig {
                    passthrough: PassthroughMode::All,
                    ..RequestHeadersConfig::default()
                },
                response: ResponseHeadersConfig::default(),
            },
            rate_limit: None,
            cors: None,
            plugin_refs: Vec::new(),
            plugins_sharing: SharingMode::Private,
        };
        tweak(&mut draft);
        let control = ControlPlaneService::new(
            self.cfg.clone(),
            self.upstreams.clone(),
            self.routes.clone(),
            self.plugins.clone(),
            self.resolver.clone(),
            Arc::new(AllowAllAuthz),
        );
        control
            .create_upstream(&ctx(tid), draft)
            .await
            .expect("seed upstream")
    }

    /// Seeds a route for `upstream_id` directly in the repo (bypasses the
    /// management validation surface; the repo still enforces the
    /// `(path_prefix, priority, method)` collision invariant).
    async fn seed_route(
        &self,
        tid: Uuid,
        upstream_id: Uuid,
        m: HttpMatch,
        priority: i32,
    ) -> crate::domain::entity::route::Route {
        let route = crate::domain::entity::route::Route {
            tenant_id: tid,
            upstream_id,
            match_type: MatchType::Http,
            match_: crate::domain::entity::route::RouteMatch::Http(m),
            priority,
            ..crate::domain::entity::route::Route::default()
        };
        self.routes.create(tid, route).await.expect("seed route")
    }
}

fn ep(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn http_match(methods: Vec<RouteMethod>, path: &str) -> HttpMatch {
    HttpMatch {
        methods,
        path_prefix: path.to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

fn problem_type(body: &Value) -> String {
    body.get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn error_source(headers: &HeaderMap) -> Option<String> {
    headers
        .get(crate::infra::error_envelope::ERROR_SOURCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Sends one request carrying the subject `SecurityContext` over `router`;
/// returns status, headers, and the raw response bytes (passthrough bodies
/// are streamed and may not be JSON).
async fn request_raw(
    router: Router,
    method: &str,
    uri: &str,
    headers: Vec<(String, String)>,
    body: Option<&str>,
    tid: Uuid,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .extension(ctx(tid));
    let has_content_length = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
    if let Some(b) = body
        && !has_content_length
    {
        builder = builder.header(header::CONTENT_LENGTH, b.len().to_string());
    }
    for (name, value) in headers {
        if let Ok(name) = axum::http::header::HeaderName::from_bytes(name.as_bytes())
            && let Ok(value) = axum::http::header::HeaderValue::from_str(&value)
        {
            builder = builder.header(name, value);
        }
    }
    let req = builder
        .body(Body::from(body.unwrap_or_default().to_owned()))
        .expect("request builds");
    let resp = router
        .clone()
        .oneshot(req)
        .await
        .expect("in-process response");
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    (status, resp_headers, bytes.to_vec())
}

/// [`request_raw`] with the body parsed as JSON (problem+json envelopes).
async fn request(
    router: Router,
    method: &str,
    uri: &str,
    headers: Vec<(String, String)>,
    body: Option<&str>,
    tid: Uuid,
) -> (StatusCode, Value, HeaderMap) {
    let (status, headers, bytes) = request_raw(router, method, uri, headers, body, tid).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value, headers)
}

/// [`request_raw`] for a GET.
async fn raw_get(router: Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    request_raw(router, "GET", uri, Vec::new(), None, subject()).await
}

// ---------------------------------------------------------------------------
// Alias resolution (algorithm `cpt-cf-oagw-algo-data-plane-proxy-resolve-alias`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alias_resolution_prefers_own_tenant_over_ancestors() {
    let resolver: Arc<dyn TenantResolverClient> = Arc::new(
        HierarchyResolver::default()
            .chain(subject(), &[parent(), root()])
            .chain(parent(), &[root()]),
    );
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let own = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Https, "10.0.0.11", 443)],
            |_| {},
        )
        .await;
    // Parent defines the same alias with a different endpoint.
    f.seed_upstream(
        parent(),
        "svc",
        vec![ep(EndpointScheme::Https, "10.0.0.12", 443)],
        |_| {},
    )
    .await;

    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let chain = vec![TenantId(subject()), TenantId(parent()), TenantId(root())];
    let (merged, leaf, chain_owned) = svc
        .resolve_alias_chain(&ctx(subject()), "svc", &chain)
        .await
        .expect("alias resolves");
    // Shadowing: the subject-level upstream wins (nearest match) while the
    // parent-level config still contributes to the effective merge chain.
    assert_eq!(leaf.id, own.id, "leaf is the subject's upstream");
    assert_eq!(leaf.server.endpoints[0].host, "10.0.0.11");
    assert_eq!(
        chain_owned.len(),
        2,
        "subject + parent both define the alias"
    );
    assert!(merged.tags.is_empty());
}

#[tokio::test]
async fn alias_resolution_falls_back_to_ancestor_when_absent_locally() {
    let resolver: Arc<dyn TenantResolverClient> = Arc::new(
        HierarchyResolver::default()
            .chain(subject(), &[parent(), root()])
            .chain(parent(), &[root()]),
    );
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    f.seed_upstream(
        parent(),
        "shared",
        vec![ep(EndpointScheme::Https, "10.0.0.12", 443)],
        |_| {},
    )
    .await;

    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let chain = vec![TenantId(subject()), TenantId(parent()), TenantId(root())];
    let (_merged, leaf, chain_owned) = svc
        .resolve_alias_chain(&ctx(subject()), "shared", &chain)
        .await
        .expect("ancestor alias resolves");
    assert_eq!(leaf.server.endpoints[0].host, "10.0.0.12");
    assert_eq!(chain_owned.len(), 1, "only the parent level contributes");
}

#[tokio::test]
async fn alias_resolution_merges_root_to_leaf_config_chain() {
    let resolver: Arc<dyn TenantResolverClient> = Arc::new(
        HierarchyResolver::default()
            .chain(subject(), &[parent(), root()])
            .chain(parent(), &[root()]),
    );
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let root_up = f
        .seed_upstream(
            root(),
            "chain",
            vec![ep(EndpointScheme::Https, "10.0.0.13", 443)],
            |d| {
                d.tags = vec!["root-tag".to_owned()];
            },
        )
        .await;
    let parent_up = f
        .seed_upstream(
            parent(),
            "chain",
            vec![ep(EndpointScheme::Https, "10.0.0.12", 443)],
            |d| {
                d.tags = vec!["parent-tag".to_owned()];
            },
        )
        .await;

    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let chain = vec![TenantId(subject()), TenantId(parent()), TenantId(root())];
    let (merged, leaf, chain_owned) = svc
        .resolve_alias_chain(&ctx(subject()), "chain", &chain)
        .await
        .expect("chain alias resolves");
    // The merged configuration is the root → leaf chain.
    assert_eq!(
        leaf.id, parent_up.id,
        "parent is the nearest defining level"
    );
    assert_ne!(root_up.id, parent_up.id);
    assert_eq!(chain_owned.len(), 2, "root + parent both contribute");
    let mut tags = merged.tags;
    tags.sort();
    assert_eq!(
        tags,
        vec!["parent-tag", "root-tag"],
        "tags union across chain"
    );
}

#[tokio::test]
async fn unknown_alias_is_404_route_not_found() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let chain = vec![TenantId(subject()), TenantId(root())];
    let err = svc
        .resolve_alias_chain(&ctx(subject()), "ghost", &chain)
        .await
        .expect_err("no such alias");
    assert_eq!(err.status(), 404);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn ancestor_disabled_upstream_dominates_the_nearest_match() {
    let resolver: Arc<dyn TenantResolverClient> = Arc::new(
        HierarchyResolver::default()
            .chain(subject(), &[parent(), root()])
            .chain(parent(), &[root()]),
    );
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    // The subject defines the alias, but an ancestor also defines it and is
    // disabled — the disabled ancestor dominates (inst-dp-dis-ancestor).
    f.seed_upstream(
        subject(),
        "svc",
        vec![ep(EndpointScheme::Https, "10.0.0.11", 443)],
        |_| {},
    )
    .await;
    f.seed_upstream(
        parent(),
        "svc",
        vec![ep(EndpointScheme::Https, "10.0.0.12", 443)],
        |d| {
            d.enabled = Some(false);
        },
    )
    .await;

    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let chain = vec![TenantId(subject()), TenantId(parent()), TenantId(root())];
    let err = svc
        .resolve_alias_chain(&ctx(subject()), "svc", &chain)
        .await
        .expect_err("ancestor-disabled must reject");
    assert_eq!(err.status(), 503);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.service.unavailable.v1"
    );
}

#[tokio::test]
async fn matched_upstream_disabled_is_503() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    f.seed_upstream(
        subject(),
        "svc",
        vec![ep(EndpointScheme::Https, "10.0.0.11", 443)],
        |d| {
            d.enabled = Some(false);
        },
    )
    .await;

    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let chain = vec![TenantId(subject()), TenantId(root())];
    let err = svc
        .resolve_alias_chain(&ctx(subject()), "svc", &chain)
        .await
        .expect_err("disabled matched upstream");
    assert_eq!(err.status(), 503);
}

// ---------------------------------------------------------------------------
// Route matching (algorithm `cpt-cf-oagw-algo-data-plane-proxy-match-route`)
// ---------------------------------------------------------------------------

/// Seeds one upstream (alias `svc`, IP endpoint) with the given routes and
/// returns the fixture plus the leaf upstream.
async fn seeded_upstream_with_routes(routes: Vec<(HttpMatch, i32)>) -> (Fixture, Upstream) {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Https, "10.0.0.11", 443)],
            |_| {},
        )
        .await;
    for (m, priority) in routes {
        f.seed_route(subject(), up.id, m, priority).await;
    }
    (f, up)
}

#[tokio::test]
async fn route_matching_empty_allowlist_rejects_all_methods() {
    // An empty method allowlist route ignores every request (inst-dp-match-*).
    let (f, _up) = seeded_upstream_with_routes(vec![(http_match(vec![], "/v1"), 0)]).await;
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let leaf = f
        .upstreams
        .find_by_alias(subject(), "svc")
        .await
        .expect("present");
    let err = svc
        .match_route(&leaf, "GET", "/v1/models")
        .await
        .expect_err("empty allowlist rejects all");
    assert_eq!(err.status(), 404);
}

#[tokio::test]
async fn route_matching_honors_the_method_allowlist() {
    let (f, _up) =
        seeded_upstream_with_routes(vec![(http_match(vec![RouteMethod::Get], "/v1"), 0)]).await;
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let leaf = f
        .upstreams
        .find_by_alias(subject(), "svc")
        .await
        .expect("present");
    // POST is not in the allowlist → no route.
    let err = svc
        .match_route(&leaf, "POST", "/v1/models")
        .await
        .expect_err("POST not allowlisted");
    assert_eq!(err.status(), 404);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    // GET matches.
    let (_route, m) = svc
        .match_route(&leaf, "GET", "/v1/models")
        .await
        .expect("GET matches");
    assert_eq!(m.methods, vec![RouteMethod::Get]);
}

#[tokio::test]
async fn route_matching_longest_path_prefix_wins() {
    let (f, _up) = seeded_upstream_with_routes(vec![
        (http_match(vec![RouteMethod::Get], "/v1"), 0),
        (http_match(vec![RouteMethod::Get], "/v1/models"), 0),
    ])
    .await;
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let leaf = f
        .upstreams
        .find_by_alias(subject(), "svc")
        .await
        .expect("present");
    let (_route, m) = svc
        .match_route(&leaf, "GET", "/v1/models/deploy")
        .await
        .expect("longest prefix matches");
    assert_eq!(m.path_prefix, "/v1/models", "longest prefix wins");
}

#[tokio::test]
async fn route_matching_priority_breaks_prefix_ties() {
    let (f, _up) = seeded_upstream_with_routes(vec![
        (http_match(vec![RouteMethod::Get], "/v1"), 5),
        (http_match(vec![RouteMethod::Get], "/v1"), 50),
    ])
    .await;
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let leaf = f
        .upstreams
        .find_by_alias(subject(), "svc")
        .await
        .expect("present");
    let (route, m) = svc
        .match_route(&leaf, "GET", "/v1/x")
        .await
        .expect("matches on priority");
    assert_eq!(route.priority, 50, "higher priority wins prefix ties");
    assert_eq!(m.path_prefix, "/v1");
}

#[tokio::test]
async fn route_matching_longest_prefix_beats_higher_priority() {
    // F-003 regression: the longest path prefix is dominant and `priority`
    // only breaks equal-prefix ties (`inst-dp-match-prefix` before
    // `inst-dp-match-priority`).  A higher-priority route with a shorter
    // prefix must NOT win over a lower-priority route with the longest prefix.
    let (f, _up) = seeded_upstream_with_routes(vec![
        (http_match(vec![RouteMethod::Get], "/v1"), 50),
        (http_match(vec![RouteMethod::Get], "/v1/models"), 0),
    ])
    .await;
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let leaf = f
        .upstreams
        .find_by_alias(subject(), "svc")
        .await
        .expect("present");
    let (_route, m) = svc
        .match_route(&leaf, "GET", "/v1/models/deploy")
        .await
        .expect("matches");
    assert_eq!(
        m.path_prefix, "/v1/models",
        "longest prefix must win over a higher-priority shorter route"
    );
}

#[tokio::test]
async fn route_matching_no_route_is_404() {
    let (f, _up) = seeded_upstream_with_routes(vec![]).await;
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let leaf = f
        .upstreams
        .find_by_alias(subject(), "svc")
        .await
        .expect("present");
    let err = svc
        .match_route(&leaf, "GET", "/nope")
        .await
        .expect_err("no routes");
    assert_eq!(err.status(), 404);
}

#[tokio::test]
async fn ac_55_request_matching_only_disabled_routes_is_route_not_found() {
    // FEATURE §55: "a request matching only disabled routes returns the
    // route-not-found outcome" (`inst-dp-match-*`).  Disabled routes are
    // excluded from matching entirely, so a request whose only matches are
    // disabled yields 404 `route.not_found` (not 503).
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Https, "10.0.0.11", 443)],
            |_| {},
        )
        .await;
    // Two routes both match GET /v1/models; both are disabled.
    for priority in [0, 1] {
        let mut route = f
            .seed_route(
                subject(),
                up.id,
                http_match(vec![RouteMethod::Get], "/v1"),
                priority,
            )
            .await;
        route.enabled = false;
        f.routes
            .update(subject(), route)
            .await
            .expect("seed disabled route");
    }

    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/models",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "only disabled routes match");
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));

    // Re-enabling one of them restores a match — the 404 came from the
    // disabled filtering, not from the path being unmatched.
    let mut routes = f.routes.list_by_upstream(subject(), up.id).await;
    routes[0].enabled = true;
    f.routes
        .update(subject(), routes[0].clone())
        .await
        .expect("re-enable route");
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let leaf = f
        .upstreams
        .find_by_alias(subject(), "svc")
        .await
        .expect("present");
    assert!(
        svc.match_route(&leaf, "GET", "/v1/models").await.is_ok(),
        "enabled route matches again"
    );
}

#[tokio::test]
async fn grpc_protocol_upstream_is_not_reachable() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    // Insert directly in the repo (the management surface rejects gRPC
    // routes, but a gRPC upstream is representable; routing is Phase 3).
    let mut up = Upstream::new(
        subject(),
        "grpc-svc",
        ServerConfig {
            endpoints: vec![ep(EndpointScheme::Https, "10.0.0.31", 443)],
        },
    );
    up.protocol = UpstreamProtocol::Grpc;
    let up = f
        .upstreams
        .create(subject(), up)
        .await
        .expect("seed grpc upstream");
    let svc = f.data_service(Arc::new(AllowAllAuthz));
    let err = svc
        .match_route(&up, "GET", "/v1/x")
        .await
        .expect_err("gRPC reserved (Phase 3)");
    assert_eq!(err.status(), 404);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

// ---------------------------------------------------------------------------
// X-OAGW-Target-Host matrix (ADR 0001)
// ---------------------------------------------------------------------------

/// A single-endpoint pool (any alias).
fn single_pool() -> Vec<Endpoint> {
    vec![ep(EndpointScheme::Https, "api.vendor.com", 443)]
}

/// A multi-endpoint pool with a derived common-suffix alias (`vendor.com`).
fn multi_common_suffix_pool() -> Vec<Endpoint> {
    vec![
        ep(EndpointScheme::Https, "us.vendor.com", 443),
        ep(EndpointScheme::Https, "eu.vendor.com", 443),
    ]
}

/// A multi-endpoint pool that is not derivable (IP hosts) → explicit alias.
fn multi_explicit_pool() -> Vec<Endpoint> {
    vec![
        ep(EndpointScheme::Https, "192.0.2.1", 443),
        ep(EndpointScheme::Https, "192.0.2.2", 443),
    ]
}

#[tokio::test]
async fn target_host_single_pool_without_header_routes_to_the_sole_endpoint() {
    let pool = single_pool();
    let selected = DataPlaneService::select_endpoint(&pool, "api.vendor.com", None)
        .expect("single pool needs no header");
    match selected {
        TargetSelection::Endpoint(ep) => assert_eq!(ep.host, "api.vendor.com"),
        TargetSelection::RoundRobin => panic!("single pool is never round-robin"),
    }
}

#[tokio::test]
async fn target_host_single_pool_with_valid_header_routes_to_it() {
    let pool = single_pool();
    let selected =
        DataPlaneService::select_endpoint(&pool, "api.vendor.com", Some("api.vendor.com"))
            .expect("header names the sole endpoint");
    match selected {
        TargetSelection::Endpoint(ep) => assert_eq!(ep.host, "api.vendor.com"),
        TargetSelection::RoundRobin => panic!("unexpected"),
    }
}

#[tokio::test]
async fn target_host_single_pool_with_unknown_header_is_400() {
    let pool = single_pool();
    let err = DataPlaneService::select_endpoint(&pool, "api.vendor.com", Some("other.com"))
        .expect_err("unknown host");
    assert_eq!(err.status(), 400);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
}

#[tokio::test]
async fn target_host_multi_explicit_pool_without_header_is_round_robin() {
    let pool = multi_explicit_pool();
    let selected = DataPlaneService::select_endpoint(&pool, "pool", None)
        .expect("explicit alias round-robins on no header");
    assert!(matches!(selected, TargetSelection::RoundRobin));
}

#[tokio::test]
async fn target_host_multi_explicit_pool_with_header_routes_to_it() {
    let pool = multi_explicit_pool();
    let selected = DataPlaneService::select_endpoint(&pool, "pool", Some("192.0.2.2"))
        .expect("header selects the endpoint");
    match selected {
        TargetSelection::Endpoint(ep) => assert_eq!(ep.host, "192.0.2.2"),
        TargetSelection::RoundRobin => panic!("unexpected"),
    }
}

#[tokio::test]
async fn target_host_multi_common_suffix_pool_requires_the_header() {
    let pool = multi_common_suffix_pool();
    let err = DataPlaneService::select_endpoint(&pool, "vendor.com", None)
        .expect_err("multi-endpoint common-suffix alias requires the header");
    assert_eq!(err.status(), 400);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
}

#[tokio::test]
async fn target_host_multi_common_suffix_pool_with_header_routes_to_it() {
    let pool = multi_common_suffix_pool();
    let selected = DataPlaneService::select_endpoint(&pool, "vendor.com", Some("eu.vendor.com"))
        .expect("header selects among the pool");
    match selected {
        TargetSelection::Endpoint(ep) => assert_eq!(ep.host, "eu.vendor.com"),
        TargetSelection::RoundRobin => panic!("unexpected"),
    }
}

#[tokio::test]
async fn target_host_invalid_format_is_400_invalid_target_host() {
    let pool = single_pool();
    for bad in ["a b", "host:443", "http://x", "a/b", ""] {
        let err = DataPlaneService::select_endpoint(&pool, "api.vendor.com", Some(bad))
            .expect_err("format must be rejected");
        assert_eq!(err.status(), 400, "bad header {bad:?}");
        assert_eq!(
            err.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
            "bad header {bad:?}"
        );
    }
}

#[tokio::test]
async fn target_host_unknown_host_on_multi_pool_is_400_unknown() {
    let pool = multi_common_suffix_pool();
    let err = DataPlaneService::select_endpoint(&pool, "vendor.com", Some("api.example.com"))
        .expect_err("unknown host not in pool");
    assert_eq!(err.status(), 400);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
}

// ---------------------------------------------------------------------------
// Framing / body guards and request policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn framing_rejects_both_content_length_and_transfer_encoding() {
    let mut headers = Headers::new();
    headers.insert("content-length", "10");
    headers.insert("transfer-encoding", "chunked");
    let err = DataPlaneService::check_framing(&headers).expect_err("CL+TE is smuggling bait");
    assert_eq!(err.status(), 400);
    assert_eq!(
        err.instance(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn framing_rejects_invalid_content_length() {
    let mut headers = Headers::new();
    headers.insert("content-length", "abc");
    let err = DataPlaneService::check_framing(&headers).expect_err("invalid CL");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn framing_returns_the_declared_length() {
    let mut headers = Headers::new();
    headers.insert("content-length", "1234");
    let len = DataPlaneService::check_framing(&headers).expect("valid CL");
    assert_eq!(len, Some(1234));
    assert_eq!(
        DataPlaneService::check_framing(&Headers::new()).expect("no CL"),
        None
    );
}

#[tokio::test]
async fn query_is_filtered_to_the_allowlist() {
    let query = vec![
        ("q".to_owned(), "models".to_owned()),
        ("limit".to_owned(), "10".to_owned()),
        ("secret".to_owned(), "x".to_owned()),
    ];
    let out = DataPlaneService::filter_query(&query, &["limit".to_owned()]);
    assert_eq!(out, vec![("limit".to_owned(), "10".to_owned())]);
    // Empty allowlist forwards nothing.
    assert!(DataPlaneService::filter_query(&query, &[]).is_empty());
}

#[tokio::test]
async fn path_suffix_disabled_rejects_a_non_empty_suffix() {
    let m = HttpMatch {
        methods: vec![RouteMethod::Get],
        path_prefix: "/v1".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Disabled,
    };
    let err = DataPlaneService::apply_path_suffix("/v1/models", &m).expect_err("suffix disabled");
    assert_eq!(err.status(), 400);
    // Exact prefix is still OK.
    let path = DataPlaneService::apply_path_suffix("/v1", &m).expect("prefix exact");
    assert_eq!(path, "/v1");
    // Append mode forwards the full path.
    let m2 = HttpMatch {
        path_suffix_mode: PathSuffixMode::Append,
        ..m.clone()
    };
    let path = DataPlaneService::apply_path_suffix("/v1/models", &m2).expect("append");
    assert_eq!(path, "/v1/models");
}

#[tokio::test]
async fn effective_rate_limit_keeps_the_minimum() {
    let stricter = |rate: u64| RateLimitConfig {
        sustained: SustainedRate {
            rate,
            window: RateLimitWindow::Minute,
        },
        ..RateLimitConfig::default()
    };
    let merged = DataPlaneService::effective_rate_limit(Some(&stricter(100)), Some(&stricter(10)));
    assert_eq!(
        merged.expect("some").sustained.rate,
        10,
        "route cannot loosen"
    );
    let none = DataPlaneService::effective_rate_limit(None, None);
    assert!(none.is_none());
}

#[test]
fn ac_27_effective_binding_order_is_upstream_then_route_contiguous_from_zero() {
    // FEATURE §27: "With upstream bindings [U1, U2] and route bindings
    // [R1, R2], execution order is [U1, U2, R1, R2] across Auth, Guards,
    // Transform(request)..."  The effective binding list layers the synthetic
    // auth binding (position 0) first, then the merged upstream plugins, then
    // the route plugins — with positions re-derived contiguous from 0.
    use crate::domain::entity::config::{PluginBinding, PluginsConfig};
    use crate::domain::entity::upstream::AuthConfig;
    use crate::domain::merge::UpstreamConfig;

    let binding = |position: u32, plugin_ref: &str| PluginBinding {
        position,
        plugin_ref: plugin_ref.to_owned(),
        plugin_uuid: None,
        config: serde_json::Value::Null,
    };

    let merged = UpstreamConfig {
        auth: Some(AuthConfig {
            plugin_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned()),
            config: serde_json::Value::Null,
            sharing: crate::domain::entity::config::SharingMode::Private,
        }),
        rate_limit: None,
        cors: None,
        plugins: PluginsConfig {
            sharing: crate::domain::entity::config::SharingMode::Inherit,
            items: vec![binding(0, "U1"), binding(1, "U2")],
        },
        tags: Vec::new(),
    };
    let route = crate::domain::entity::route::Route {
        plugins: PluginsConfig {
            sharing: crate::domain::entity::config::SharingMode::Inherit,
            items: vec![binding(0, "R1"), binding(1, "R2")],
        },
        ..Default::default()
    };

    let bindings = DataPlaneService::effective_bindings(&merged, &route);
    let refs: Vec<String> = bindings.iter().map(|b| b.plugin_ref.clone()).collect();
    assert_eq!(
        refs,
        vec![
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "U1",
            "U2",
            "R1",
            "R2",
        ],
        "synthetic auth, then [U1, U2], then [R1, R2]"
    );
    for (index, b) in bindings.iter().enumerate() {
        assert_eq!(b.position as usize, index, "positions contiguous from 0");
        assert_eq!(b.plugin_uuid, None, "built-in refs carry no uuid");
    }
}

// ---------------------------------------------------------------------------
// The proxy hot path over HTTP (permissions, CORS, framing, rate limiting)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_unknown_alias_is_404_problem_gateway() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, headers) = request(
        router,
        "GET",
        "/proxy/ghost/v1/models",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}

#[tokio::test]
async fn proxy_denied_permission_is_403_access_denied() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    // Authorization is step 1, so no upstream/route is needed for a denial.
    let router = f.router(Arc::new(DenyAuthz));
    let (status, body, headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/models",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.access.denied.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn proxy_pdp_failure_is_503_service_unavailable() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let router = f.router(Arc::new(FailAuthz));
    let (status, body, _) = request(
        router,
        "GET",
        "/proxy/svc/v1/models",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.service.unavailable.v1"
    );
}

/// Seeds a multi-endpoint common-suffix upstream (alias `vendor.com`).
async fn seed_vendor_com_upstream(f: &Fixture) {
    let up = f
        .seed_upstream(subject(), "vendor.com", multi_common_suffix_pool(), |_| {})
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;
}

#[tokio::test]
async fn proxy_missing_target_host_is_400() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    seed_vendor_com_upstream(&f).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, _) = request(
        router,
        "GET",
        "/proxy/vendor.com/v1/models",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
}

#[tokio::test]
async fn proxy_invalid_target_host_is_400() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    seed_vendor_com_upstream(&f).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, _) = request(
        router,
        "GET",
        "/proxy/vendor.com/v1/models",
        vec![(TARGET_HOST_HEADER.to_owned(), "not a host".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
}

#[tokio::test]
async fn proxy_unknown_target_host_is_400() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    seed_vendor_com_upstream(&f).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, _) = request(
        router,
        "GET",
        "/proxy/vendor.com/v1/models",
        vec![(TARGET_HOST_HEADER.to_owned(), "api.example.com".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
}

#[tokio::test]
async fn proxy_rejects_both_content_length_and_transfer_encoding() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Https, "192.0.2.1", 443)],
            |_| {},
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Post], "/v1"),
        0,
    )
    .await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, _) = request(
        router,
        "POST",
        "/proxy/svc/v1",
        vec![("transfer-encoding".to_owned(), "chunked".to_owned())],
        Some("hello"),
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|d| d.contains("Content-Length"))
    );
}

#[tokio::test]
async fn proxy_body_over_the_100mb_cap_is_413() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Https, "192.0.2.1", 443)],
            |_| {},
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Post], "/v1"),
        0,
    )
    .await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, headers) = request(
        router,
        "POST",
        "/proxy/svc/v1",
        vec![(
            "content-length".to_owned(),
            (MAX_BODY_BYTES + 1).to_string(),
        )],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn proxy_rate_limit_429_carries_projections() {
    let (port, _captured) = spawn_upstream(200, Vec::new(), b"hello".to_vec()).await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let limit = RateLimitConfig {
        sharing: SharingMode::Private,
        sustained: SustainedRate {
            rate: 1,
            window: RateLimitWindow::Second,
        },
        burst: Some(BurstConfig { capacity: Some(1) }),
        ..RateLimitConfig::default()
    };
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |d| {
                d.rate_limit = Some(limit);
            },
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;
    let router = f.router(Arc::new(AllowAllAuthz));
    // First request is within the 1-token budget and succeeds.
    let (s1, _, _) = request(
        router.clone(),
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first request within budget");
    // Second request exhausts the bucket → 429 with the projections.
    let (status, body, headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert!(
        headers.get("x-ratelimit-limit").is_some(),
        "X-RateLimit-Limit"
    );
    assert!(
        headers.get("x-ratelimit-remaining").is_some(),
        "X-RateLimit-Remaining"
    );
    assert!(
        headers.get("x-ratelimit-reset").is_some(),
        "X-RateLimit-Reset"
    );
    assert!(headers.get(header::RETRY_AFTER).is_some(), "Retry-After");
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn proxy_cors_violation_is_a_raw_403_with_the_adr_type() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Https, "192.0.2.1", 443)],
            |d| {
                d.cors = Some(CorsConfig {
                    sharing: SharingMode::Private,
                    enabled: true,
                    allowed_origins: vec!["https://good.example.com".to_owned()],
                    ..CorsConfig::default()
                });
            },
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/models",
        vec![("origin".to_owned(), "https://evil.example.com".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body.get("type").and_then(Value::as_str),
        Some("gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1")
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert!(
        headers
            .get_all(header::VARY)
            .iter()
            .any(|v| v.to_str().map(|s| s == "Origin").unwrap_or(false)),
        "Vary: Origin present on the CORS rejection"
    );
}

#[tokio::test]
async fn route_cors_cannot_loosen_an_ancestor_enforced_origin_set() {
    // F-004 regression (`inst-dp-cfg-enforce`): a route's CORS overlays the
    // tenant-chain effective CORS — it must not *replace* it.  When the
    // ancestor effective set is enforce-resolved, the route may only tighten;
    // an origin the enforced base does not allow must stay rejected even if
    // the route lists it.
    let (port, _captured) = spawn_upstream(200, Vec::new(), b"ok".to_vec()).await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    // Ancestor (root) enforces CORS to a single origin.
    f.seed_upstream(
        root(),
        "svc",
        vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
        |d| {
            d.cors = Some(CorsConfig {
                sharing: SharingMode::Enforce,
                enabled: true,
                allowed_origins: vec!["https://a.com".to_owned()],
                allow_credentials: true,
                ..CorsConfig::default()
            });
        },
    )
    .await;
    // Leaf owns its own upstream (nearest match) whose CORS inherits.
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |d| {
                d.cors = Some(CorsConfig {
                    sharing: SharingMode::Inherit,
                    enabled: true,
                    allowed_origins: vec!["https://b.com".to_owned()],
                    ..CorsConfig::default()
                });
            },
        )
        .await;
    // The route tries to widen the enforced set to include b.com.
    let route = crate::domain::entity::route::Route {
        tenant_id: subject(),
        upstream_id: up.id,
        match_: crate::domain::entity::route::RouteMatch::Http(http_match(
            vec![RouteMethod::Get],
            "/v1",
        )),
        cors: Some(CorsConfig {
            sharing: SharingMode::Inherit,
            enabled: true,
            allowed_origins: vec!["https://a.com".to_owned(), "https://b.com".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        }),
        priority: 0,
        ..crate::domain::entity::route::Route::default()
    };
    f.routes.create(subject(), route).await.expect("seed route");

    let router = f.router(Arc::new(AllowAllAuthz));
    // Origin outside the enforced set stays rejected (403) despite the route
    // listing it — the old code replaced the merged set with the route's and
    // admitted b.com here.
    let (status, body, _headers) = request(
        router.clone(),
        "GET",
        "/proxy/svc/v1/models",
        vec![("origin".to_owned(), "https://b.com".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "route must not widen enforced CORS"
    );
    assert_eq!(
        body.get("type").and_then(Value::as_str),
        Some("gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1")
    );
    // The enforced origin still passes through the route CORS overlay.
    let (status, _body, _headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/models",
        vec![("origin".to_owned(), "https://a.com".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enforced origin still allowed");
}

#[tokio::test]
async fn proxy_preflight_answers_204_with_cors_headers() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, _, headers) = request(
        router,
        "OPTIONS",
        "/proxy/svc/v1/models",
        vec![
            ("origin".to_owned(), "https://app.example.com".to_owned()),
            (
                "access-control-request-method".to_owned(),
                "POST".to_owned(),
            ),
            (
                "access-control-request-headers".to_owned(),
                "X-Custom".to_owned(),
            ),
        ],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|v| v.to_str().ok()),
        Some("X-Custom")
    );
    assert!(
        headers.get(header::ACCESS_CONTROL_MAX_AGE).is_some(),
        "Access-Control-Max-Age present"
    );
}

#[tokio::test]
async fn proxy_bare_options_is_404() {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(false, 2), resolver);
    let router = f.router(Arc::new(AllowAllAuthz));
    // Origin but no Access-Control-Request-Method → not a preflight → 404.
    let (status, body, headers) = request(
        router,
        "OPTIONS",
        "/proxy/svc/v1",
        vec![("origin".to_owned(), "https://app.example.com".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

// ---------------------------------------------------------------------------
// Hyper forwarding + response passthrough (ADR 0001 host rewrite, ADR 0007)
// ---------------------------------------------------------------------------

/// One request as observed by an upstream test server.
#[derive(Debug, Default)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == &name.to_ascii_lowercase())
            .map(|(_, v)| v.as_str())
    }
}

/// Spawns a one-shot HTTP/1.1 upstream server over loopback that records the
/// request and replies with `status` + `extra_headers` + `body`.  Returns the
/// port and the capture handle.
async fn spawn_upstream(
    status: u16,
    extra_headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> (u16, Arc<Mutex<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let captured = Arc::new(Mutex::new(CapturedRequest::default()));
    let cap = Arc::clone(&captured);
    tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let head_len = loop {
            let n = socket.read(&mut tmp).await.expect("read head");
            assert!(n > 0, "upstream EOF before request head");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_len]);
        let mut lines = head.split("\r\n");
        let request_line = lines.next().expect("request line");
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().expect("method").to_owned();
        let path = request_parts.next().expect("path").to_owned();
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut content_length: Option<usize> = None;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let name = k.trim().to_ascii_lowercase();
                let value = v.trim().to_owned();
                if name == "content-length" {
                    content_length = value.parse().ok();
                }
                headers.push((name, value));
            }
        }
        if let Some(len) = content_length {
            while buf.len() < head_len + len {
                let n = socket.read(&mut tmp).await.expect("read body");
                assert!(n > 0, "upstream EOF before body");
                buf.extend_from_slice(&tmp[..n]);
            }
        }
        let body_bytes = buf[head_len..head_len + content_length.unwrap_or(0)].to_vec();
        {
            let mut cap = cap.lock().expect("capture lock");
            cap.method = method;
            cap.path = path;
            cap.headers = headers;
            cap.body = body_bytes;
        }
        let reason = match status {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
        for (k, v) in &extra_headers {
            response.push_str(&format!("{k}: {v}\r\n"));
        }
        response.push_str(&format!("Content-Length: {}\r\n", body.len()));
        response.push_str("Connection: close\r\n\r\n");
        let mut bytes = response.into_bytes();
        bytes.extend_from_slice(&body);
        socket.write_all(&bytes).await.expect("write response");
    });
    (port, captured)
}

/// Like [`spawn_upstream`] but serves `count` sequential requests on the same
/// listener (each recorded into the capture and answered with `status`), for
/// tests that make several successful proxy hits against one upstream.
async fn spawn_upstream_many(
    count: usize,
    status: u16,
    extra_headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> (u16, Arc<Mutex<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let captured = Arc::new(Mutex::new(CapturedRequest::default()));
    let cap = Arc::clone(&captured);
    tokio::spawn(async move {
        for _ in 0..count {
            let (mut socket, _peer) = listener.accept().await.expect("accept");
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            let head_len = loop {
                let n = socket.read(&mut tmp).await.expect("read head");
                assert!(n > 0, "upstream EOF before request head");
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_len]);
            let mut lines = head.split("\r\n");
            let request_line = lines.next().expect("request line");
            let mut request_parts = request_line.split_whitespace();
            let method = request_parts.next().expect("method").to_owned();
            let path = request_parts.next().expect("path").to_owned();
            let mut headers: Vec<(String, String)> = Vec::new();
            let mut content_length: Option<usize> = None;
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    let name = k.trim().to_ascii_lowercase();
                    let value = v.trim().to_owned();
                    if name == "content-length" {
                        content_length = value.parse().ok();
                    }
                    headers.push((name, value));
                }
            }
            if let Some(len) = content_length {
                while buf.len() < head_len + len {
                    let n = socket.read(&mut tmp).await.expect("read body");
                    assert!(n > 0, "upstream EOF before body");
                    buf.extend_from_slice(&tmp[..n]);
                }
            }
            let body_bytes = buf[head_len..head_len + content_length.unwrap_or(0)].to_vec();
            {
                let mut cap = cap.lock().expect("capture lock");
                cap.method = method;
                cap.path = path;
                cap.headers = headers;
                cap.body = body_bytes;
            }
            let reason = match status {
                200 => "OK",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
            for (k, v) in &extra_headers {
                response.push_str(&format!("{k}: {v}\r\n"));
            }
            response.push_str(&format!("Content-Length: {}\r\n", body.len()));
            response.push_str("Connection: close\r\n\r\n");
            let mut bytes = response.into_bytes();
            bytes.extend_from_slice(&body);
            socket.write_all(&bytes).await.expect("write response");
        }
    });
    (port, captured)
}

/// Spawns a loopback listener that accepts, reads the request head, and then
/// stays silent (for the `proxy_timeout_secs` 504 test).
async fn spawn_silent() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        if let Ok((mut socket, _peer)) = listener.accept().await {
            let mut tmp = [0u8; 4096];
            let _ = socket.read(&mut tmp).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    port
}

/// Reserves a port with nothing listening (for the connect-refused 503 test).
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

/// Seeds a single-endpoint HTTP upstream bound to `port` with a GET `/v1`
/// route, returning the fixture.
async fn seed_http_forward(port: u16) -> Fixture {
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |_| {},
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;
    f
}

#[tokio::test]
async fn proxy_forwards_and_streams_the_upstream_response() {
    let (port, captured) = spawn_upstream(
        200,
        vec![("X-Upstream".to_owned(), "hello".to_owned())],
        b"chunk-1 chunk-2".to_vec(),
    )
    .await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |d| {
                d.cors = Some(CorsConfig {
                    sharing: SharingMode::Inherit,
                    enabled: true,
                    allowed_origins: vec!["https://app.example.com".to_owned()],
                    ..CorsConfig::default()
                });
            },
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        HttpMatch {
            methods: vec![RouteMethod::Get],
            path_prefix: "/v1".to_owned(),
            query_allowlist: vec!["q".to_owned()],
            path_suffix_mode: PathSuffixMode::Append,
        },
        0,
    )
    .await;

    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, headers, body) = request_raw(
        router,
        "GET",
        "/proxy/svc/v1/echo?q=models",
        vec![("origin".to_owned(), "https://app.example.com".to_owned())],
        None,
        subject(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "200 from upstream passthrough");
    assert_eq!(body, b"chunk-1 chunk-2", "streamed body untouched");
    assert_eq!(
        headers
            .get(crate::infra::error_envelope::ERROR_SOURCE_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    assert_eq!(
        headers.get("x-upstream").and_then(|v| v.to_str().ok()),
        Some("hello"),
        "upstream headers pass through"
    );
    // CORS actual headers applied on the passthrough.
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert!(
        headers
            .get_all(header::VARY)
            .iter()
            .any(|v| v.to_str().ok() == Some("Origin")),
        "Vary: Origin on the actual response"
    );

    let cap = captured.lock().expect("capture");
    assert_eq!(cap.method, "GET");
    assert_eq!(
        cap.path, "/v1/echo?q=models",
        "path + allowlisted query forwarded"
    );
    let rewritten_host = format!("127.0.0.1:{port}");
    assert_eq!(
        cap.header("host"),
        Some(rewritten_host.as_str()),
        "Host rewritten to the endpoint authority"
    );
}

#[tokio::test]
async fn proxy_upstream_4xx_passes_through_unchanged() {
    let (port, _captured) = spawn_upstream(
        404,
        vec![("Content-Type".to_owned(), "text/plain".to_owned())],
        b"no such resource".to_vec(),
    )
    .await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, headers, body) = raw_get(router, "/proxy/svc/v1/echo").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, b"no such resource", "upstream 404 body untouched");
    assert_eq!(error_source(&headers).as_deref(), Some("upstream"));
    // No problem+json wrap on upstream failures (ADR 0007).
    assert_ne!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}

#[tokio::test]
async fn proxy_upstream_500_passes_through_unchanged() {
    let (port, _captured) = spawn_upstream(
        500,
        vec![("Content-Type".to_owned(), "text/plain".to_owned())],
        b"upstream exploded".to_vec(),
    )
    .await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, headers, body) = raw_get(router, "/proxy/svc/v1/echo").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, b"upstream exploded");
    assert_eq!(error_source(&headers).as_deref(), Some("upstream"));
}

#[tokio::test]
async fn ac_55_hop_by_hop_headers_stripped_and_header_transforms_applied() {
    // FEATURE §55 (`inst-dp-hdr-strip`, `inst-dp-hdr-transform`): hop-by-hop
    // headers and the gateway routing/attribution headers must never reach
    // the upstream, while the request `set`/`add`/`remove` transforms are
    // applied on top of the passthrough set.
    let (port, captured) = spawn_upstream(200, Vec::new(), b"ok".to_vec()).await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |d| {
                d.headers.request.set =
                    BTreeMap::from([("X-Transform-Set".to_owned(), "set-v1".to_owned())]);
                d.headers.request.add =
                    BTreeMap::from([("X-Transform-Add".to_owned(), "add-v1".to_owned())]);
                d.headers.request.remove = vec!["x-remove-me".to_owned()];
                // Response transform: set + strip, verified on the way back.
                d.headers.response.set =
                    BTreeMap::from([("X-Resp-Set".to_owned(), "resp-v1".to_owned())]);
                d.headers.response.remove = vec!["x-internal".to_owned()];
            },
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;

    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, _headers, _body) = request_raw(
        router,
        "GET",
        "/proxy/svc/v1",
        vec![
            ("connection".to_owned(), "keep-alive".to_owned()),
            ("keep-alive".to_owned(), "timeout=5".to_owned()),
            (
                "proxy-authorization".to_owned(),
                "Basic dXNlcjpwYXNz".to_owned(),
            ),
            ("te".to_owned(), "trailers".to_owned()),
            ("trailer".to_owned(), "X-Checksum".to_owned()),
            ("upgrade".to_owned(), "websocket".to_owned()),
            ("x-oagw-target-host".to_owned(), "127.0.0.1".to_owned()),
            ("x-oagw-error-source".to_owned(), "gateway".to_owned()),
            ("x-remove-me".to_owned(), "gone".to_owned()),
            ("x-forwarded-for".to_owned(), "192.0.2.1".to_owned()),
        ],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let cap = captured.lock().expect("capture");
    // Hop-by-hop headers stripped from the outbound request (hyper re-adds
    // its own `connection` framing, so the stable assertions are the other
    // seven names).
    for stripped in [
        "keep-alive",
        "proxy-authorization",
        "te",
        "trailer",
        "upgrade",
    ] {
        assert!(
            !cap.headers.iter().any(|(k, _)| k == stripped),
            "hop-by-hop header '{stripped}' must not reach the upstream"
        );
    }
    // Gateway routing/attribution headers are consumed by the gateway.
    for stripped in ["x-oagw-target-host", "x-oagw-error-source"] {
        assert!(
            !cap.headers.iter().any(|(k, _)| k == stripped),
            "routing header '{stripped}' must not reach the upstream"
        );
    }
    // Request transforms landed; the passthrough header survived.
    assert_eq!(cap.header("x-transform-set"), Some("set-v1"));
    assert_eq!(cap.header("x-transform-add"), Some("add-v1"));
    assert_eq!(cap.header("x-forwarded-for"), Some("192.0.2.1"));
}

#[test]
fn ac_55_header_matrix_strips_hop_by_hop_and_applies_set_add_remove() {
    // The header matrix (algorithm `cpt-cf-oagw-algo-data-plane-proxy-headers`)
    // at the function level: every hop-by-hop name is removed, the
    // passthrough modes select the forwarded set, and the `set`/`add`/`remove`
    // request transforms are applied.  (The wire-level plugin overlay only
    // re-layers plugin-produced headers; the matrix itself is what strips.)
    let mut hs: Headers = vec![
        ("Connection".to_owned(), "keep-alive".to_owned()),
        ("Keep-Alive".to_owned(), "timeout=5".to_owned()),
        ("Proxy-Authenticate".to_owned(), "Basic a".to_owned()),
        ("Proxy-Authorization".to_owned(), "Basic b".to_owned()),
        ("TE".to_owned(), "trailers".to_owned()),
        ("Trailer".to_owned(), "X-C".to_owned()),
        ("Transfer-Encoding".to_owned(), "chunked".to_owned()),
        ("Upgrade".to_owned(), "websocket".to_owned()),
        ("X-OK".to_owned(), "v".to_owned()),
    ]
    .into_iter()
    .collect();
    DataPlaneService::strip_hop_by_hop(&mut hs);
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        assert!(!hs.contains(name), "hop-by-hop '{name}' stripped");
    }
    assert!(hs.contains("x-ok"), "non-hop-by-hop header kept");

    // set/add/remove transforms over the passthrough set.
    let inbound: Headers = vec![
        ("x-keep".to_owned(), "1".to_owned()),
        ("x-remove-me".to_owned(), "gone".to_owned()),
        ("x-forwarded-for".to_owned(), "192.0.2.1".to_owned()),
    ]
    .into_iter()
    .collect();
    let mut cfg = RequestHeadersConfig {
        passthrough: PassthroughMode::All,
        set: BTreeMap::from([("X-Set".to_owned(), "v".to_owned())]),
        add: BTreeMap::from([("X-Add".to_owned(), "w".to_owned())]),
        remove: vec!["x-remove-me".to_owned()],
        ..RequestHeadersConfig::default()
    };
    let mut out = DataPlaneService::apply_passthrough_policy(&inbound, &cfg);
    DataPlaneService::apply_request_transforms(&mut out, &cfg);
    assert!(
        !out.contains("x-remove-me"),
        "remove transform drops the header"
    );
    assert_eq!(out.get("x-set"), Some("v"), "set transform applied");
    assert_eq!(out.get("x-add"), Some("w"), "add transform applied");
    assert_eq!(out.get("x-keep"), Some("1"), "passthrough kept");

    // passthrough None forwards nothing beyond the transforms.
    cfg.passthrough = PassthroughMode::None;
    let out = DataPlaneService::apply_passthrough_policy(&inbound, &cfg);
    assert!(!out.contains("x-keep") && !out.contains("x-forwarded-for"));
    // passthrough Allowlist forwards only the listed names.
    cfg.passthrough = PassthroughMode::Allowlist;
    cfg.passthrough_allowlist = vec!["x-keep".to_owned()];
    let out = DataPlaneService::apply_passthrough_policy(&inbound, &cfg);
    assert!(out.contains("x-keep") && !out.contains("x-forwarded-for"));
}

#[tokio::test]
async fn ac_55_crlf_in_outbound_header_value_is_rejected() {
    // FEATURE §55: "A header value containing CR or LF is rejected"
    // (`inst-dp-hdr-crlf`).  A transform-set value carrying CR/LF must be
    // rejected with 400 before the socket opens (HTTP smuggling defense).
    let (port, _captured) = spawn_upstream(200, Vec::new(), b"ok".to_vec()).await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |d| {
                d.headers.request.set =
                    BTreeMap::from([("X-Evil".to_owned(), "value\r\nX-Injected: 1".to_owned())]);
            },
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;

    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, headers) =
        request(router, "GET", "/proxy/svc/v1", Vec::new(), None, subject()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "CRLF header is rejected");
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));

    // The header-matrix guard itself also rejects a CR/LF directly.
    let mut bad = Headers::new();
    bad.insert("x-name", "a\nb");
    let err = DataPlaneService::check_crlf(&bad).expect_err("LF rejected");
    assert_eq!(err.status(), 400);
    let mut bad_cr = Headers::new();
    bad_cr.insert("x-name", "a\rb");
    assert!(DataPlaneService::check_crlf(&bad_cr).is_err());
    assert!(DataPlaneService::check_crlf(&Headers::new()).is_ok());
}

#[tokio::test]
async fn ac_55_plaintext_upstream_is_blocked_when_allow_http_is_false() {
    // FEATURE §55: "A plaintext upstream request is blocked when
    // `allow_http_upstream` is false" (`inst-dp-ssrf-scheme`).  The hot-path
    // SSRF re-check rejects the `http` endpoint under the current policy even
    // when the record predates the policy (records persisted with
    // `allow_http_upstream=true`, service now running with the guard on).
    let port = free_port().await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |_| {},
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;

    // A DataPlaneService running under the tightened policy (`allow_http=false`)
    // over the same repositories.
    let mut cfg = f.cfg.clone();
    cfg.allow_http_upstream = false;
    let control = ControlPlaneService::new(
        cfg.clone(),
        f.upstreams.clone(),
        f.routes.clone(),
        f.plugins.clone(),
        f.resolver.clone(),
        Arc::new(AllowAllAuthz),
    );
    let data = DataPlaneService::new(
        cfg,
        f.upstreams.clone(),
        f.routes.clone(),
        f.plugins.clone(),
        builtin_registries_with_cache(Some(f.credstore.clone()), Duration::from_secs(60), 100),
        f.rate_limiter.clone(),
        f.credstore.clone(),
        Arc::new(MockTypesRegistryClient::new()),
        f.resolver.clone(),
        Arc::new(AllowAllAuthz),
        Arc::clone(&f.metrics),
    );
    let registry = toolkit::api::OpenApiRegistryImpl::new();
    let router = crate::api::rest::register_routes(
        Router::new(),
        &registry,
        Arc::new(GearState {
            control,
            data,
            metrics: Arc::clone(&f.metrics),
        }),
    )
    .expect("routes register");

    let (status, body, headers) =
        request(router, "GET", "/proxy/svc/v1", Vec::new(), None, subject()).await;
    // The SSRF guard fires before the socket opens — 503 link.unavailable
    // (never a connect-refused attribution, and no upstream contact).
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("allow_http_upstream=true"),
        "detail names the SSRF policy: {detail}"
    );
}

#[tokio::test]
async fn ac_55_upstream_protocol_parse_failure_is_502_protocol_error() {
    // FEATURE §55: "An upstream protocol parse failure returns 502
    // `...cf.oagw.protocol.error.v1`" — an upstream that answers with bytes
    // that are not a valid HTTP/1.x response head surfaces as the gateway
    // `protocol.error` (502), distinct from the connect-refused
    // `link.unavailable` (503).
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        let mut tmp = [0u8; 4096];
        let _ = socket.read(&mut tmp).await;
        // Mangled response head — not a valid HTTP version/status line.
        let _ = socket
            .write_all(b"THIS IS NOT HTTP\r\nstill garbage\r\n\r\n")
            .await;
    });

    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "parse failure is 502");
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn proxy_timeout_is_504_gateway() {
    let port = spawn_silent().await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn proxy_connect_refused_is_503_link_unavailable() {
    let port = free_port().await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn proxy_injects_apikey_credentials_into_the_outbound_request() {
    let (port, captured) = spawn_upstream(200, Vec::new(), b"authed".to_vec()).await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_credstore(
        test_cfg(true, 2),
        resolver,
        vec![("partner-key".to_owned(), "sk-secret".to_owned())],
    );
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |d| {
                d.auth = AuthConfig {
                    plugin_type: Some(APIKEY_AUTH.to_owned()),
                    sharing: SharingMode::Private,
                    config: json!({
                        "header_name": "X-API-Key",
                        "value_ref": "cred://partner-key",
                    }),
                };
            },
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, _, _body) = raw_get(router, "/proxy/svc/v1/echo").await;
    assert_eq!(status, StatusCode::OK);
    let cap = captured.lock().expect("capture");
    assert_eq!(
        cap.header("x-api-key"),
        Some("sk-secret"),
        "apikey injected"
    );
    assert_eq!(cap.method, "GET");
}

#[tokio::test]
async fn passthrough_none_is_not_defeated_by_the_plugin_overlay() {
    // F-001 regression: with `passthrough=None` (the production default —
    // "forward no inbound headers beyond defaults") plus a
    // `remove:["x-secret"]` transform, the plugin overlay must NOT re-layer
    // the client-supplied inbound set into the outbound request.  Only
    // headers the plugin chain actually *added* survive the overlay.
    let (port, captured) = spawn_upstream(200, Vec::new(), b"ok".to_vec()).await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_credstore(
        test_cfg(true, 2),
        resolver,
        vec![("partner-key".to_owned(), "sk-secret".to_owned())],
    );
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |d| {
                d.headers.request.passthrough = PassthroughMode::None;
                d.headers.request.remove = vec!["x-secret".to_owned()];
                // Even an allowlisted name must not leak under `None`.
                d.headers.request.passthrough_allowlist = vec!["x-keep".to_owned()];
                d.auth = AuthConfig {
                    plugin_type: Some(APIKEY_AUTH.to_owned()),
                    sharing: SharingMode::Private,
                    config: json!({
                        "header_name": "X-API-Key",
                        "value_ref": "cred://partner-key",
                    }),
                };
            },
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, _headers, _body) = request_raw(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        vec![
            ("x-secret".to_owned(), "topsecret".to_owned()),
            ("authorization".to_owned(), "Bearer client-token".to_owned()),
            ("cookie".to_owned(), "session=abc".to_owned()),
            ("x-keep".to_owned(), "allowlisted".to_owned()),
        ],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "request proxied");
    let cap = captured.lock().expect("capture");
    for name in ["x-secret", "authorization", "cookie", "x-keep"] {
        assert!(
            cap.header(name).is_none(),
            "client-supplied header '{name}' leaked to the upstream under passthrough=None"
        );
    }
    // The plugin-injected credential still arrives: the overlay re-layers only
    // chain-added headers.
    assert_eq!(
        cap.header("x-api-key"),
        Some("sk-secret"),
        "plugin-injected header must survive the matrix"
    );
}

// ---------------------------------------------------------------------------
// Observability & Audit (feature `cpt-cf-oagw-feature-observability-audit`,
// p5; flow `cpt-cf-oagw-flow-observability-audit-record-metrics` /
// `cpt-cf-oagw-flow-observability-audit-correlate`; DoD
// `cpt-cf-oagw-dod-observability-audit-correlation` /
// `cpt-cf-oagw-dod-observability-audit-error-source`)
// ---------------------------------------------------------------------------

use crate::infra::audit::AUDIT_TARGET;
use crate::infra::metrics::{
    ENDPOINT_SELECTED, ERRORS_TOTAL, RATE_LIMIT_EXCEEDED, RATE_LIMIT_USAGE, REQUESTS_IN_FLIGHT,
    REQUESTS_TOTAL, TARGET_HOST_USED, UPSTREAM_AVAILABLE,
};

/// The GTS error instance for a gateway rate-limit rejection (the
/// `error_type` label on the errors counter and the audit `error_type`).
const RL_EXCEEDED_INSTANCE: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
/// The GTS error instance for an unmatched alias (`http.route=unmatched`).
const ROUTE_NOT_FOUND_INSTANCE: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
/// The GTS error instance for a connect-refused upstream (link.unavailable).
const LINK_UNAVAILABLE_INSTANCE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";

#[tokio::test]
async fn proxy_metrics_move_for_success_404_and_429() {
    let (port, _captured) = spawn_upstream(200, Vec::new(), b"hello".to_vec()).await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let limit = RateLimitConfig {
        sharing: SharingMode::Private,
        sustained: SustainedRate {
            rate: 1,
            window: RateLimitWindow::Second,
        },
        burst: Some(BurstConfig { capacity: Some(1) }),
        ..RateLimitConfig::default()
    };
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |d| {
                d.rate_limit = Some(limit);
            },
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;
    let router = f.router(Arc::new(AllowAllAuthz));

    // 1. Success: 200 counters + duration histogram + in-flight back to 0.
    let (s1, _, _) = request(
        router.clone(),
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first request within budget");
    assert_eq!(
        f.metrics.counter_value(
            REQUESTS_TOTAL,
            &[
                ("host", "svc"),
                ("http.request.method", "GET"),
                ("http.route", "/v1"),
                ("http.response.status_code", "200"),
            ],
        ),
        1,
        "success increments requests_total (host svc, route /v1, 200)"
    );
    assert_eq!(
        f.metrics
            .histogram_count(&[("host", "svc"), ("http.route", "/v1"), ("phase", "total")]),
        1,
        "success observes the duration histogram"
    );
    assert_eq!(
        f.metrics
            .gauge_value(REQUESTS_IN_FLIGHT, &[("host", "svc")]),
        Some(0.0),
        "in-flight returns to 0 after completion"
    );

    // 2. Rate limit: second request exhausts the budget → 429.
    let (s2, body, _) = request(
        router.clone(),
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(problem_type(&body), RL_EXCEEDED_INSTANCE);
    assert_eq!(
        f.metrics.counter_value(
            REQUESTS_TOTAL,
            &[
                ("host", "svc"),
                ("http.request.method", "GET"),
                ("http.route", "/v1"),
                ("http.response.status_code", "429"),
            ],
        ),
        1,
        "429 increments requests_total with status 429"
    );
    assert_eq!(
        f.metrics.counter_value(
            RATE_LIMIT_EXCEEDED,
            &[("host", "svc"), ("path", "/v1/echo")]
        ),
        1,
        "429 increments oagw_rate_limit_exceeded_total"
    );
    let ratio = f
        .metrics
        .gauge_value(RATE_LIMIT_USAGE, &[("host", "svc"), ("path", "/v1/echo")])
        .expect("usage ratio observed on the rejection");
    assert!(
        (0.0..=1.0).contains(&ratio),
        "usage ratio within the DESIGN [0.0, 2.0] window (got {ratio})"
    );
    assert_eq!(
        f.metrics.counter_value(
            ERRORS_TOTAL,
            &[
                ("host", "svc"),
                ("http.route", "/v1"),
                ("error_type", RL_EXCEEDED_INSTANCE),
                ("error_source", "gateway"),
            ],
        ),
        1,
        "gateway rate-limit rejection labels the errors counter"
    );

    // 3. Unknown alias → 404 route-not-found (route label `unmatched`).
    let (s3, body3, _) =
        request(router, "GET", "/proxy/nope/v1", Vec::new(), None, subject()).await;
    assert_eq!(s3, StatusCode::NOT_FOUND);
    assert_eq!(problem_type(&body3), ROUTE_NOT_FOUND_INSTANCE);
    assert_eq!(
        f.metrics.counter_value(
            REQUESTS_TOTAL,
            &[
                ("host", "nope"),
                ("http.request.method", "GET"),
                ("http.route", "unmatched"),
                ("http.response.status_code", "404"),
            ],
        ),
        1,
        "404 before routing labels http.route=unmatched"
    );
    assert_eq!(
        f.metrics.counter_value(
            ERRORS_TOTAL,
            &[
                ("host", "nope"),
                ("http.route", "unmatched"),
                ("error_type", ROUTE_NOT_FOUND_INSTANCE),
                ("error_source", "gateway"),
            ],
        ),
        1,
        "gateway 404 labels the errors counter"
    );
    assert_eq!(
        f.metrics
            .gauge_value(REQUESTS_IN_FLIGHT, &[("host", "svc")]),
        Some(0.0),
        "in-flight drained after the 429"
    );
}

#[tokio::test]
async fn proxy_metrics_record_routing_selection_and_upstream_availability() {
    let (port, _captured) = spawn_upstream_many(2, 200, Vec::new(), b"ok".to_vec()).await;
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let up = f
        .seed_upstream(
            subject(),
            "svc",
            vec![ep(EndpointScheme::Http, "127.0.0.1", port)],
            |_| {},
        )
        .await;
    f.seed_route(
        subject(),
        up.id,
        http_match(vec![RouteMethod::Get], "/v1"),
        0,
    )
    .await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let upstream_id = up.id;

    // Explicit X-OAGW-Target-Host names the sole endpoint → `explicit_header`.
    let (s1, _, _) = request(
        router.clone(),
        "GET",
        "/proxy/svc/v1/echo",
        vec![(TARGET_HOST_HEADER.to_owned(), "127.0.0.1".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(
        f.metrics.counter_value(
            TARGET_HOST_USED,
            &[
                ("upstream_id", &upstream_id.to_string()),
                ("endpoint_host", "127.0.0.1"),
            ],
        ),
        1,
        "explicit target host increments oagw_routing_target_host_used"
    );
    assert_eq!(
        f.metrics.counter_value(
            ENDPOINT_SELECTED,
            &[
                ("upstream_id", &upstream_id.to_string()),
                ("endpoint_host", "127.0.0.1"),
                ("selection_method", "explicit_header"),
            ],
        ),
        1,
        "explicit header selection recorded"
    );

    // Without the header a single-endpoint pool falls back to `default`.
    let (s2, _, _) = request(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        f.metrics.counter_value(
            ENDPOINT_SELECTED,
            &[
                ("upstream_id", &upstream_id.to_string()),
                ("endpoint_host", "127.0.0.1"),
                ("selection_method", "default"),
            ],
        ),
        1,
        "no-header selection records `default`"
    );

    // Both exchanges completed against the live loopback → availability 1.
    assert_eq!(
        f.metrics.gauge_value(
            UPSTREAM_AVAILABLE,
            &[("host", "svc"), ("endpoint", "127.0.0.1")]
        ),
        Some(1.0),
        "successful exchange marks the upstream available"
    );
}

#[tokio::test]
async fn proxy_metrics_attribute_upstream_5xx_to_the_upstream_source() {
    let (port, _captured) = spawn_upstream(500, Vec::new(), b"boom".to_vec()).await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, _body, headers) = request(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_source(&headers).as_deref(), Some("upstream"));

    assert_eq!(
        f.metrics.counter_value(
            REQUESTS_TOTAL,
            &[
                ("host", "svc"),
                ("http.request.method", "GET"),
                ("http.route", "/v1"),
                ("http.response.status_code", "500"),
            ],
        ),
        1,
        "upstream 500 passes through and counts"
    );
    assert_eq!(
        f.metrics.counter_value(
            ERRORS_TOTAL,
            &[
                ("host", "svc"),
                ("http.route", "/v1"),
                ("error_type", "upstream_http_error"),
                ("error_source", "upstream"),
            ],
        ),
        1,
        "upstream 5xx is attributed to source=upstream (ADR 0007)"
    );
    assert_eq!(
        f.metrics.gauge_value(
            UPSTREAM_AVAILABLE,
            &[("host", "svc"), ("endpoint", "127.0.0.1")]
        ),
        Some(1.0),
        "an HTTP 500 is still a completed upstream response → available"
    );
}

#[tokio::test]
async fn proxy_metrics_record_connect_refused_as_gateway_error_and_down() {
    let port = free_port().await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, body, _) = request(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(problem_type(&body), LINK_UNAVAILABLE_INSTANCE);

    assert_eq!(
        f.metrics.counter_value(
            ERRORS_TOTAL,
            &[
                ("host", "svc"),
                ("http.route", "/v1"),
                ("error_type", LINK_UNAVAILABLE_INSTANCE),
                ("error_source", "gateway"),
            ],
        ),
        1,
        "connect-refused is attributed to source=gateway"
    );
    assert_eq!(
        f.metrics.gauge_value(
            UPSTREAM_AVAILABLE,
            &[("host", "svc"), ("endpoint", "127.0.0.1")]
        ),
        Some(0.0),
        "a failed link marks the upstream unavailable"
    );
}

#[tokio::test]
async fn proxy_correlates_the_request_id_end_to_end() {
    let (port, captured) = spawn_upstream_many(2, 200, Vec::new(), b"ok".to_vec()).await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));

    // Client-supplied id is reused: echoed on the response and forwarded
    // upstream (flow `cpt-cf-oagw-flow-observability-audit-correlate`,
    // `inst-ob-cor-propagate`).
    let (status, _, headers) = request(
        router.clone(),
        "GET",
        "/proxy/svc/v1/echo",
        vec![("x-request-id".to_owned(), "req-abc-123".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-request-id").and_then(|v| v.to_str().ok()),
        Some("req-abc-123"),
        "the correlated id is echoed to the caller (`inst-ob-cor-record`)"
    );
    assert_eq!(
        captured.lock().expect("capture").header("x-request-id"),
        Some("req-abc-123"),
        "the correlated id propagates to the upstream"
    );

    // Gateway-minted id when the client supplies none: minted at entry
    // (`inst-ob-cor-relid`), echoed, and propagated in lockstep.
    let (s2, _, h2) = request(
        router.clone(),
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    let minted = h2
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let minted = minted.expect("gateway mints a request id");
    assert!(!minted.is_empty());
    assert_eq!(
        captured.lock().expect("capture").header("x-request-id"),
        Some(minted.as_str()),
        "the minted id reaches the upstream"
    );

    // The error envelope and the response carry the same correlated id
    // (`inst-ob-cor-record`).
    let (s3, body, h3) = request(
        router,
        "GET",
        "/proxy/nope/v1",
        vec![("x-request-id".to_owned(), "req-404-xyz".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(s3, StatusCode::NOT_FOUND);
    assert_eq!(
        body.get("request_id").and_then(Value::as_str),
        Some("req-404-xyz")
    );
    assert_eq!(
        h3.get("x-request-id").and_then(|v| v.to_str().ok()),
        Some("req-404-xyz"),
        "gateway failures still echo the request id"
    );
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_after_a_proxy_hit() {
    let (port, _captured) = spawn_upstream(200, Vec::new(), b"ok".to_vec()).await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));

    // Unauthenticated (no SecurityContext extension) → 401 `auth.failed`.
    let unauthenticated = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .expect("request builds");
    let resp = router
        .clone()
        .oneshot(unauthenticated)
        .await
        .expect("in-process response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(
        problem_type(&body),
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
        "unauthenticated /metrics is rejected with the auth.failed instance"
    );

    // Authenticated with a proxy hit on the shared registry → 200 with the
    // DESIGN §4.2 exposition (DoD `cpt-cf-oagw-dod-observability-audit-metrics`).
    let (status, _, _) = request(
        router.clone(),
        "GET",
        "/proxy/svc/v1/echo",
        Vec::new(),
        None,
        subject(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "seed a series into the shared registry"
    );
    let (ms, headers, bytes) = raw_get(router, "/metrics").await;
    assert_eq!(ms, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; version=0.0.4")
    );
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("oagw_requests_total{host=\"svc\",http.request.method=\"GET\",http.response.status_code=\"200\",http.route=\"/v1\"} 1"),
        "prometheus exposition carries the observed series:\n{text}"
    );
    assert!(
        text.contains("oagw_request_duration_seconds_bucket"),
        "histogram buckets are exposed"
    );
    assert!(
        text.contains("# TYPE oagw_requests_total counter")
            && text.contains("# HELP oagw_errors_total")
            && text.contains("# TYPE oagw_requests_in_flight gauge"),
        "HELP/TYPE metadata is exposed"
    );
}

// ---------------------------------------------------------------------------
// Audit-log capture (DESIGN §4.3; DoD
// `cpt-cf-oagw-dod-observability-audit-audit-log`, `inst-ob-al-*`)
// ---------------------------------------------------------------------------

/// One capturable audit event: the level plus the deterministic DESIGN §4.3
/// JSON record (`AuditEntry::to_json`).
#[derive(Clone)]
struct CapturedAudit {
    level: String,
    json: Value,
}

/// Minimal `tracing::Subscriber` gathering only events on the `oagw::audit`
/// target.  The oagw crate intentionally depends on `tracing` alone (no
/// tracing-subscriber in the lockfile), so the tests synthesize the observer
/// side.
struct CapturingSubscriber {
    sink: Arc<Mutex<Vec<CapturedAudit>>>,
}

impl tracing::Subscriber for CapturingSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == AUDIT_TARGET
    }
    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(0)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() != AUDIT_TARGET {
            return;
        }
        let mut visitor = FieldVisitor { fields: Vec::new() };
        event.record(&mut visitor);
        let json = visitor
            .fields
            .iter()
            .find(|(name, _)| name == "json")
            .map(|(_, rendered)| serde_json::from_str::<Value>(rendered).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);
        self.sink
            .lock()
            .expect("audit sink lock")
            .push(CapturedAudit {
                level: event.metadata().level().as_str().to_owned(),
                json,
            });
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Collects event field values as rendered strings (the `json` field is
/// recorded with `%`, so `record_debug` yields the raw JSON text).
struct FieldVisitor {
    fields: Vec<(String, String)>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .push((field.name().to_owned(), value.to_owned()));
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_owned(), format!("{value:?}")));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }
    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }
}

/// Installs the capturing subscriber once per process and returns the shared
/// sink.  A global subscriber can only be installed once; if something else
/// already installed one, the sink stays empty and the assertions fail
/// loudly rather than silently passing.
fn audit_capture() -> Arc<Mutex<Vec<CapturedAudit>>> {
    static SINK: std::sync::OnceLock<Arc<Mutex<Vec<CapturedAudit>>>> = std::sync::OnceLock::new();
    Arc::clone(SINK.get_or_init(|| {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            sink: Arc::clone(&sink),
        };
        let _ = tracing::subscriber::set_global_default(subscriber);
        sink
    }))
}

/// Waits (yielding) for the DESIGN JSON record of the audit entry carrying
/// `request_id` and returns it.  Concurrent tests populate the shared sink,
/// so entries are isolated by their correlated id; the sink guard is never
/// held across a comparison or a panic (a poisoned shared mutex would take
/// the whole suite down).
async fn wait_for_audit_record(
    capture: &Arc<Mutex<Vec<CapturedAudit>>>,
    request_id: &str,
) -> Value {
    for _ in 0..200 {
        let snapshot: Vec<CapturedAudit> = {
            let captured = capture.lock().expect("audit sink lock");
            captured
                .iter()
                .filter(|entry| {
                    entry.json.get("request_id").and_then(Value::as_str) == Some(request_id)
                })
                .cloned()
                .collect()
        };
        if let Some(entry) = snapshot.into_iter().last() {
            assert_eq!(entry.level, "INFO", "audit level for {request_id}");
            return entry.json;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no audit entry for request_id {request_id}")
}

#[tokio::test]
async fn audit_proxy_entry_carries_the_design_field_set() {
    let capture = audit_capture();
    let (port, _captured) = spawn_upstream(200, Vec::new(), b"ok".to_vec()).await;
    let f = seed_http_forward(port).await;
    let router = f.router(Arc::new(AllowAllAuthz));
    let (status, _, _) = request(
        router,
        "GET",
        "/proxy/svc/v1/echo",
        vec![("x-request-id".to_owned(), "audit-proxy-1".to_owned())],
        None,
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let record = wait_for_audit_record(&capture, "audit-proxy-1").await;
    assert_eq!(record["event"], json!("proxy.request.completed"));
    assert_eq!(record["level"], json!("INFO"));
    assert_eq!(record["host"], json!("svc"));
    assert_eq!(record["path"], json!("/proxy/svc/v1/echo"));
    assert_eq!(record["method"], json!("GET"));
    assert_eq!(record["status"], json!(200));
    assert_eq!(record["tenant_id"], json!(subject().to_string()));
    assert_eq!(
        record["principal_id"],
        json!(tenant_id(0xaaaa_aaaa_aaaa_aaaa).to_string())
    );
    assert!(
        record["request_size"].is_null(),
        "GET carries no content-length (got {record})"
    );
    assert!(
        record["duration_ms"].as_u64().is_some(),
        "duration_ms is populated (got {record})"
    );
    assert_eq!(
        record["error_type"],
        json!(""),
        "error_type empty on success"
    );
    let ts = record["timestamp"].as_str().expect("RFC 3339 timestamp");
    assert!(
        ts.ends_with('Z') && ts.contains('T'),
        "timestamp is UTC RFC 3339 (got {ts})"
    );
}

#[tokio::test]
async fn audit_config_change_emitted_on_upstream_create() {
    let capture = audit_capture();
    let resolver: Arc<dyn TenantResolverClient> =
        Arc::new(HierarchyResolver::default().chain(subject(), &[root()]));
    let f = Fixture::with_resolver(test_cfg(true, 2), resolver);
    let router = f.router(Arc::new(AllowAllAuthz));

    let body = json!({
        "alias": "aud-svc",
        "server": {
            "endpoints": [ { "scheme": "https", "host": "192.0.2.10", "port": 443 } ]
        },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    let (status, _resp, _headers) = request(
        router,
        "POST",
        "/upstreams",
        vec![
            ("x-request-id".to_owned(), "audit-create-1".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ],
        Some(&body.to_string()),
        subject(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let record = wait_for_audit_record(&capture, "audit-create-1").await;
    assert_eq!(record["event"], json!("config.upstream.created"));
    assert_eq!(record["level"], json!("INFO"));
    assert_eq!(record["path"], json!("/upstreams"));
    assert_eq!(record["method"], json!("POST"));
    assert_eq!(record["status"], json!(201));
    assert_eq!(record["tenant_id"], json!(subject().to_string()));
    assert_eq!(
        record["principal_id"],
        json!(tenant_id(0xaaaa_aaaa_aaaa_aaaa).to_string())
    );
    assert!(record.get("host").is_none(), "no host on config entries");
    assert_eq!(
        record["error_type"],
        json!(""),
        "error_type empty on success"
    );
}
