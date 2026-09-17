//! Data Plane service — the proxy hot path (feature
//! `cpt-cf-oagw-feature-data-plane-proxy`, p5; flow
//! `cpt-cf-oagw-flow-data-plane-proxy-execute`; sequence
//! `cpt-cf-oagw-seq-proxy-flow`; ADR 0001 request routing; ADR 0007
//! error-source distinction).
//!
//! Inbound requests whose path begins with `/proxy` are dispatched here (flow
//! `cpt-cf-oagw-flow-gear-foundation-plane-routing`).
//! [`DataPlaneService::proxy`] orchestrates the whole data-plane hot path
//! (flow `cpt-cf-oagw-flow-data-plane-proxy-execute`, steps `inst-dp-exec-*`):
//!
//! 1. **Authorization** — the `gts.cf.core.oagw.proxy.v1~:invoke` permission
//!    is checked through the Policy Enforcer before anything else
//!    (`inst-dp-exec-authz`);
//! 2. **Framing/body guards** — `Content-Length`/`Transfer-Encoding`
//!    consistency and the 100 MB body cap (`inst-dp-ssrf-te`, `inst-dp-ssrf-body`);
//! 3. **Alias resolution** — descendant-to-root chain walking with shadowing;
//!    an ancestor-disabled upstream dominates the nearest match (algorithm
//!    `cpt-cf-oagw-algo-data-plane-proxy-resolve-alias`, steps
//!    `inst-dp-alias-*`, `inst-dp-dis-*`);
//! 4. **Route matching** — enabled routes, method allowlist (empty rejects
//!    all), longest path prefix, priority tiebreak (algorithm
//!    `cpt-cf-oagw-algo-data-plane-proxy-match-route`, steps `inst-dp-match-*`);
//! 5. **Effective-config merge** — the tenant-chain base (root → leaf) with
//!    route overrides layered on top (algorithm
//!    `cpt-cf-oagw-algo-data-plane-proxy-apply-config`);
//! 6. **Rate limiting** — the post-merge effective bucket (the bucket is keyed
//!    on the resolved upstream/route, a data dependency that pins the check
//!    after resolution) — `inst-dp-exec-rl`;
//! 7. **CORS actual-request enforcement** (`inst-dp-exec-cors`, ADR 0004);
//! 8. **Plugin chain** — auth → guards(request) → transform(request)
//!    (`inst-dp-exec-plugins`);
//! 9. **Target-host selection** (`inst-dp-thx-*`, the `X-OAGW-Target-Host`
//!    matrix of ADR 0001), header transforms and hop-by-hop stripping, SSRF
//!    re-checks, then forward the streaming body over hyper with the
//!    `proxy_timeout_secs` policy and **no retries**
//!    (`inst-dp-exec-forward`, `inst-dp-exec-timeout`, `inst-dp-pass-noretry`);
//! 10. **Response passthrough** — stream, run response transforms, attribute
//!     `X-OAGW-Error-Source` (`inst-dp-exec-stream`, `inst-dp-exec-return`).
//!
//! Error attribution follows ADR 0007 (DoD
//! `cpt-cf-oagw-dod-error-semantics-source`): gateway-generated failures are
//! RFC 9457 problem+json envelopes tagged `X-OAGW-Error-Source: gateway`;
//! upstream-produced responses (including 4xx/5xx) pass through unchanged
//! tagged `upstream` — never wrapped in a gateway envelope.
//!
//! # Observability (feature `cpt-cf-oagw-feature-observability-audit`)
//!
//! Every proxy invocation records the DESIGN §4.2 series into the shared
//! [`MetricsRegistry`]: per-request counters/duration/in-flight in
//! [`DataPlaneService::proxy`], rate-limit exceedances and usage ratio at
//! step 6, routing (target-host used, endpoint selected) at step 9, and
//! upstream availability in [`DataPlaneService::forward`].  The recording
//! is label-set lookup plus atomics, after the request completes — nothing
//! blocks or lives on the request/response path.
//!
//! # Transport note (TLS)
//!
//! The outbound transport is hyper's HTTP/1.1 client over the plain
//! [`HttpConnector`] — no TLS library is in the locked dependency set for this
//! crate.  HTTPS upstreams remain validated at the configuration boundary and
//! on the hot path (SSRF scheme allowlist: TLS schemes always, plaintext
//! `http` only behind `allow_http_upstream`), but actual TLS termination for
//! `https` endpoints is expected at the platform/ingress boundary; this
//! delivery exercises `http` endpoints in tests (`allow_http_upstream`).
//! WSS / WebTransport / gRPC endpoints are rejected by the HTTP proxy
//! transport (gRPC routing is Phase 3, DoD
//! `cpt-cf-oagw-dod-domain-model-repositories-grpc-reserved`).

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverClient;
use authz_resolver_sdk::pep::{AccessRequest, PolicyEnforcer};
use axum::body::Body as AxumBody;
use axum::http::header::{HeaderName, HeaderValue};
use axum::http::{HeaderMap, Response, Uri};
use bytes::Bytes;
use credstore_sdk::CredStoreClientV1;
use hyper::Request as HyperRequest;
use hyper::body::{Body as HttpBody, Frame, Incoming as HyperIncoming, SizeHint};
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tenant_resolver_sdk::TenantResolverClient;
use tenant_resolver_sdk::models::{GetAncestorsOptions, TenantId};
use toolkit_security::{SecurityContext, pep_properties};
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::cors::{CorsHeaders, CorsOutcome, CorsViolation, evaluate_actual};
use crate::domain::entity::alias::{
    compute_derived_alias, normalize_alias, standard_port, validate_endpoint_url,
    validate_rfc1123_hostname,
};
use crate::domain::entity::config::{
    CorsConfig, EndpointScheme, PassthroughMode, PluginBinding, RateLimitConfig,
    RequestHeadersConfig, ResponseHeadersConfig, SharingMode, UpstreamProtocol,
};
use crate::domain::entity::route::{HttpMatch, PathSuffixMode, Route};
use crate::domain::entity::upstream::{Endpoint, Upstream};
use crate::domain::error::{DomainError, ErrorSource};
use crate::domain::merge::{UpstreamConfig, merge_cors_union};
use crate::domain::plugin::chain::ResponseOutcome;
use crate::domain::plugin::registries::CustomPluginLookup;
use crate::domain::plugin::{
    ChainOutcome, ErrorContext, Headers, PluginChain, PluginRegistries, RequestContext,
    ResponseContext,
};
use crate::domain::rate::{Decider, RateLimitDecision, RateLimitInfo, RateLimitRequest};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository, http_match_of};
use crate::domain::service::control_plane::map_enforcer_err;
use crate::infra::metrics::{InFlightGuard, MetricsRegistry};

/// The `X-OAGW-Target-Host` routing header (ADR 0001).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Maximum upstream request body size — 100 MB (FEATURE `inst-dp-ssrf-body`).
/// Enforced by a streaming body wrapper plus a `Content-Length` pre-check;
/// both map to 413 `payload.too_large`.
pub const MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;

/// Hop-by-hop headers stripped on request and response forwarding
/// (`inst-dp-hdr-strip`).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Routing/attribution headers consumed by the gateway and never forwarded
/// (`inst-dp-hdr-osrc`).
const ROUTING_HEADERS: &[&str] = &["x-oagw-target-host", "x-oagw-error-source"];

/// GTS resource types and actions for data-plane authorization (DESIGN §3.3,
/// flow `inst-dp-exec-authz`).  The proxy resource is the instance-prefix form
/// `gts.cf.core.oagw.proxy.v1~`; the sole action is `invoke`.
pub mod authz {
    use authz_resolver_sdk::pep::ResourceType;
    use toolkit_security::pep_properties;

    /// `gts.cf.core.oagw.proxy.v1~` resource type.
    pub const PROXY: ResourceType = ResourceType::from_static(
        "gts.cf.core.oagw.proxy.v1~",
        &[pep_properties::OWNER_TENANT_ID],
    );

    /// The `invoke` action on the proxy resource.
    pub const INVOKE: &str = "invoke";
}

/// Downstream target context attached to gateway-error envelopes (the OAGW
/// `upstream_id` / `host` / `path` extension fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetContext {
    /// The matched upstream's id.
    pub upstream_id: Uuid,
    /// The outbound authority host:port dialed for the upstream.
    pub host: String,
    /// The outbound path forwarded to the upstream.
    pub path: String,
}

/// A data-plane failure with the response decorations the handler needs
/// (`X-RateLimit-*`/`Retry-After` on 429s, CORS headers on cross-origin
/// actual responses, and the raw CORS 403 serving the `cf.oagw.cors.*` GTS
/// instance, see [`CorsViolation`]).
#[derive(Debug)]
pub struct ProxyFailure {
    /// The gateway error (rendered as an RFC 9457 envelope with
    /// `X-OAGW-Error-Source: gateway`).
    pub error: DomainError,
    /// A CORS rejection.  When present the handler serves the raw 403 with
    /// the full `cf.oagw.cors.*` GTS instance instead of `error` (ADR 0004;
    /// `error` carries the matching [`DomainError`] variant for the audit
    /// attribution in `crate::domain::cors`).
    pub cors_violation: Option<CorsViolation>,
    /// Rate-limit projections for the `X-RateLimit-*`/`Retry-After` headers.
    pub rate_limit: Option<RateLimitInfo>,
    /// The downstream target context for the envelope extensions.
    pub target: Option<TargetContext>,
    /// CORS response headers to apply to the rendered error response (set
    /// when the actual request had already been CORS-allowed).
    pub cors_headers: Vec<(String, String)>,
}

impl ProxyFailure {
    /// Builds a bare gateway failure.
    #[must_use]
    pub fn gateway(error: DomainError) -> Self {
        Self {
            error,
            cors_violation: None,
            rate_limit: None,
            target: None,
            cors_headers: Vec::new(),
        }
    }

    /// Builds a gateway failure carrying the downstream target context.
    #[must_use]
    pub fn gateway_with_target(error: DomainError, target: TargetContext) -> Self {
        Self {
            error,
            cors_violation: None,
            rate_limit: None,
            target: Some(target),
            cors_headers: Vec::new(),
        }
    }

    /// Sets the rate-limit info (429 rejections).
    #[must_use]
    pub fn with_rate_limit(mut self, info: RateLimitInfo) -> Self {
        self.rate_limit = Some(info);
        self
    }

    /// Sets the CORS response headers (allowed cross-origin requests).
    #[must_use]
    pub fn with_cors_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.cors_headers = headers;
        self
    }

    /// Builds a CORS-violation failure.  `error` carries the matching
    /// `cf.oagw.cors.*` [`DomainError`] (403) for the audit attribution, and
    /// the handler serves the raw 403 with the same full GTS instance (see
    /// [`CorsViolation`] docs).
    #[must_use]
    pub fn cors(violation: CorsViolation) -> Self {
        Self {
            error: violation.to_domain_error(),
            cors_violation: Some(violation),
            rate_limit: None,
            target: None,
            cors_headers: Vec::new(),
        }
    }
}

/// Inbound proxy request surface extracted by the API handler.
#[derive(Debug)]
pub struct ProxyRequest {
    /// HTTP method token (upper-cased).
    pub method: String,
    /// The request path as received (`/proxy/{alias}[/{rest}]`) — used for
    /// error `instance` correlation.
    pub path: String,
    /// Normalized target alias (lower-cased, trailing dots stripped).
    pub alias: String,
    /// Path under the alias, always starting with `/` (`""` becomes `/`).
    pub rest_path: String,
    /// Query parameters as raw `(name, value)` pairs.
    pub query: Vec<(String, String)>,
    /// Inbound headers (lower-cased names).
    pub headers: Headers,
    /// Inbound request body (streamed, never buffered).
    pub body: AxumBody,
}

/// Data Plane service — outbound proxy hot path.
pub struct DataPlaneService {
    /// Resolved gear configuration (proxy timeout, SSRF policy).
    pub cfg: OagwConfig,
    /// Upstream repository (alias resolution by target alias).
    pub upstreams: Arc<dyn UpstreamRepository>,
    /// Route repository (match-rule lookup for the matched upstream).
    pub routes: Arc<dyn RouteRepository>,
    /// Plugin repository (also the custom-plugin registry for the chain).
    pub plugins: Arc<dyn PluginRepository>,
    /// The assembled built-in plugin registries (auth / guards / transforms).
    pub registries: PluginRegistries,
    /// In-process rate-limiting decider (feature
    /// `cpt-cf-oagw-feature-rate-limiting`).
    pub rate_limiter: Arc<dyn Decider>,
    /// Credential vault client (auth plugin credential resolution).
    pub credstore: Arc<dyn CredStoreClientV1>,
    /// Types registry client (plugin instance / type resolution).
    pub types_registry: Arc<dyn TypesRegistryClient>,
    /// Tenant hierarchy client (alias resolution chain walk).
    pub tenant_resolver: Arc<dyn TenantResolverClient>,
    /// Authorization client backing the PEP (`proxy:invoke`).
    pub authz_resolver: Arc<dyn AuthZResolverClient>,
    /// Policy enforcer built over `authz_resolver`.
    enforcer: PolicyEnforcer,
    /// Round-robin counter for multi-endpoint explicit-alias upstreams.
    rr: AtomicUsize,
    /// Hyper HTTP/1.1 client used for upstream forwarding (no retries).
    hyper: LegacyClient<HttpConnector, CapBody>,
    /// The shared DESIGN §4.2 metrics registry (feature
    /// `cpt-cf-oagw-feature-observability-audit`).
    metrics: Arc<MetricsRegistry>,
}

impl DataPlaneService {
    /// Assembles the Data Plane service and its PEP / hyper client.
    ///
    /// The hyper client is configured for **single-attempt** forwarding:
    /// `retry_canceled_requests` is disabled (the legacy client otherwise
    /// retries a canceled in-flight request on a reused pool connection) and
    /// `set_host` is disabled because the header matrix rewrites `Host`
    /// explicitly (`inst-dp-pass-noretry`).
    //
    // The constructor takes the assembled dependency graph (repositories,
    // SDK clients, limiter, registries, metrics); each argument is a distinct
    // atomic dependency, so the long parameter list is intentional.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        cfg: OagwConfig,
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        registries: PluginRegistries,
        rate_limiter: Arc<dyn Decider>,
        credstore: Arc<dyn CredStoreClientV1>,
        types_registry: Arc<dyn TypesRegistryClient>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        authz_resolver: Arc<dyn AuthZResolverClient>,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        let enforcer = PolicyEnforcer::new(authz_resolver.clone());
        let mut connector = HttpConnector::new();
        // Accept both `http` and `https` schemes at the connector; scheme and
        // host are SSRF-checked at selection time (see module docs for the
        // TLS-termination boundary).
        connector.enforce_http(false);
        let mut builder = LegacyClient::builder(TokioExecutor::new());
        builder.retry_canceled_requests(false);
        builder.set_host(false);
        let hyper: LegacyClient<HttpConnector, CapBody> = builder.build(connector);
        Self {
            cfg,
            upstreams,
            routes,
            plugins,
            registries,
            rate_limiter,
            credstore,
            types_registry,
            tenant_resolver,
            authz_resolver,
            enforcer,
            rr: AtomicUsize::new(0),
            hyper,
            metrics,
        }
    }

    /// Authorizes the `gts.cf.core.oagw.proxy.v1~:invoke` permission for the
    /// subject context (flow `inst-dp-exec-authz`; the access request pins
    /// `owner_tenant_id` to the subject tenant and requires non-empty
    /// constraints so a misconfigured PDP cannot widen access).
    async fn authorize_invoke(&self, ctx: &SecurityContext) -> Result<(), DomainError> {
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .require_constraints(true);
        self.enforcer
            .access_scope_with(ctx, &authz::PROXY, authz::INVOKE, None, &request)
            .await
            .map(|_| ())
            .map_err(map_enforcer_err)
    }

    // ---------------------------------------------------------------------
    // Alias resolution (algorithm `cpt-cf-oagw-algo-data-plane-proxy-resolve-alias`)
    // ---------------------------------------------------------------------

    /// Resolves the nearest upstream for `alias` across the tenant chain
    /// (descendant → root, shadowing; `inst-dp-alias-*`).
    ///
    /// `chain` is ordered **subject → root** (`[subject, parent, ..., root]`).
    /// The nearest (lowest index) tenant defining the alias wins
    /// (`inst-dp-alias-nearest`); the effective configuration chain is the
    /// set of levels from the root-most defining one down to the matched
    /// tenant, ordered **root → leaf** (an ancestor above the match still
    /// contributes its config).  Fails with:
    /// - 404 `route.not_found` when no level defines the alias
    ///   (`inst-dp-alias-notfound`);
    /// - 503 `service.unavailable` when the matched upstream **or any
    ///   ancestor** that defines the alias is disabled — the ancestor-disabled
    ///   case dominates (`inst-dp-dis-ancestor`, `inst-dp-alias-enforced`).
    ///
    /// Returns the merged tenant-chain configuration (root → leaf), the leaf
    /// (matched) upstream, and the contributing upstream chain.
    async fn resolve_alias_chain(
        &self,
        _ctx: &SecurityContext,
        alias: &str,
        chain: &[TenantId],
    ) -> Result<(UpstreamConfig, Upstream, Vec<Upstream>), DomainError> {
        let normalized = normalize_alias(alias);

        // Look up the alias at every level of the chain (subject first).
        let mut per_level: Vec<Option<Upstream>> = Vec::with_capacity(chain.len());
        for tenant in chain {
            per_level.push(self.upstreams.find_by_alias(tenant.0, &normalized).await);
        }

        // Nearest match (smallest index).  None → 404 (inst-dp-alias-notfound).
        let nearest = per_level.iter().position(Option::is_some).ok_or_else(|| {
            DomainError::RouteNotFound {
                detail: format!(
                    "no upstream resolved for alias '{}' in this tenant chain",
                    normalized
                ),
            }
        })?;

        // Ancestor-disabled dominance: any level at or above the nearest match
        // defining the alias and disabled rejects the request (disabled
        // dominates regardless of shadowing — inst-dp-dis-ancestor).
        for candidate in per_level[nearest..].iter().flatten() {
            if !candidate.enabled {
                return Err(DomainError::ServiceUnavailable {
                    detail: format!(
                        "upstream '{}' (alias '{}') is disabled",
                        candidate.id, normalized
                    ),
                    retry_after: None,
                    cause: None,
                });
            }
        }

        // Effective merge chain: every defining level from `nearest` up to the
        // root-most defining level, ordered root → leaf (merge_chain order).
        let mut chain_refs: Vec<&Upstream> = Vec::new();
        for candidate in per_level[nearest..].iter().rev().flatten() {
            chain_refs.push(candidate);
        }
        let merged = UpstreamConfig::merge_chain(&chain_refs);
        // The nearest match above guarantees at least one contributing level,
        // so the chain is non-empty by construction (no panic path here).
        let leaf = match chain_refs.last() {
            Some(leaf_ref) => (*leaf_ref).clone(),
            None => {
                return Err(DomainError::RouteNotFound {
                    detail: format!(
                        "no upstream resolved for alias '{}' in this tenant chain",
                        normalized
                    ),
                });
            }
        };
        let chain_owned: Vec<Upstream> = chain_refs.into_iter().cloned().collect();
        Ok((merged, leaf, chain_owned))
    }

    // ---------------------------------------------------------------------
    // Route matching (algorithm `cpt-cf-oagw-algo-data-plane-proxy-match-route`)
    // ---------------------------------------------------------------------

    /// Matches `method`/`rest_path` against the upstream's enabled routes
    /// (`inst-dp-match-*`): only enabled routes, method allowlist (an empty
    /// allowlist rejects all methods), longest path prefix, then priority.
    /// `None` → 404 `route.not_found`.
    async fn match_route(
        &self,
        leaf: &Upstream,
        method: &str,
        rest_path: &str,
    ) -> Result<(Route, HttpMatch), DomainError> {
        if leaf.protocol != UpstreamProtocol::Http {
            // gRPC routing is reserved / Phase 3 (DoD
            // `cpt-cf-oagw-dod-domain-model-repositories-grpc-reserved`).
            return Err(DomainError::RouteNotFound {
                detail: format!(
                    "upstream '{}' uses a non-HTTP protocol; gRPC routing is reserved (Phase 3)",
                    leaf.alias
                ),
            });
        }
        let routes = self.routes.list_by_upstream(leaf.tenant_id, leaf.id).await;
        let mut best: Option<(i32, usize, Route, HttpMatch)> = None;
        for route in routes.iter().filter(|r| r.enabled) {
            let Ok(m) = http_match_of(route) else {
                continue;
            };
            // Method allowlist: an empty allowlist rejects all methods.
            if m.methods.is_empty() {
                continue;
            }
            if !m.methods.iter().any(|rm| rm.as_str() == method) {
                continue;
            }
            // Longest path prefix on the request path.
            if !rest_path.starts_with(m.path_prefix.as_str()) {
                continue;
            }
            // Longest prefix wins; `priority` only breaks equal-prefix ties
            // (`inst-dp-match-prefix` before `inst-dp-match-priority`).
            let candidate = (
                route.priority,
                m.path_prefix.len(),
                route.clone(),
                m.clone(),
            );
            let replace = match &best {
                None => true,
                Some((best_prio, best_len, _, _)) => {
                    candidate.1 > *best_len
                        || (candidate.1 == *best_len && candidate.0 > *best_prio)
                }
            };
            if replace {
                best = Some(candidate);
            }
        }
        best.map(|(_, _, route, hmatch)| (route, hmatch))
            .ok_or_else(|| DomainError::RouteNotFound {
                detail: format!(
                    "no route matches {} '{}' for upstream '{}'",
                    method, rest_path, leaf.alias
                ),
            })
    }

    // ---------------------------------------------------------------------
    // Endpoint selection (algorithm `cpt-cf-oagw-algo-data-plane-proxy-target-host`)
    // ---------------------------------------------------------------------

    /// Applies the `X-OAGW-Target-Host` behavior matrix (ADR 0001;
    /// steps `inst-dp-thx-*`):
    ///
    /// | Endpoints | Alias        | Header | Behavior |
    /// |-----------|--------------|--------|----------|
    /// | 1         | any          | no     | the sole endpoint |
    /// | 1         | any          | yes    | validate, route to it |
    /// | 2+        | explicit     | no     | round-robin |
    /// | 2+        | explicit     | yes    | route to the named endpoint |
    /// | 2+        | common suffix| no     | 400 `missing_target_host` |
    /// | 2+        | common suffix| yes    | route to the named endpoint |
    fn select_endpoint<'a>(
        endpoints: &'a [Endpoint],
        alias: &str,
        target_header: Option<&str>,
    ) -> Result<TargetSelection<'a>, DomainError> {
        match (endpoints.len(), target_header) {
            (0, _) => Err(DomainError::validation(
                Some("server.endpoints"),
                format!("upstream '{}' has no endpoints", alias),
            )),
            (1, None) => Ok(TargetSelection::Endpoint(&endpoints[0])),
            (1, Some(header)) => {
                Self::validate_target_host(header)?;
                if endpoint_has_host(&endpoints[0], header) {
                    Ok(TargetSelection::Endpoint(&endpoints[0]))
                } else {
                    Err(DomainError::UnknownTargetHost {
                        host: header.to_owned(),
                    })
                }
            }
            (_, Some(header)) => {
                Self::validate_target_host(header)?;
                match endpoints.iter().find(|ep| endpoint_has_host(ep, header)) {
                    Some(ep) => Ok(TargetSelection::Endpoint(ep)),
                    None => Err(DomainError::UnknownTargetHost {
                        host: header.to_owned(),
                    }),
                }
            }
            (_, None) => {
                if compute_derived_alias(endpoints).as_deref() == Some(alias) {
                    // Multi-endpoint common-suffix alias: the header is
                    // required (inst-dp-thx-required).
                    Err(DomainError::MissingTargetHost {
                        detail: format!(
                            "X-OAGW-Target-Host is required for alias '{}'; \
                             valid hosts: {}",
                            alias,
                            known_hosts(endpoints)
                        ),
                    })
                } else {
                    // Multi-endpoint explicit alias: round-robin.
                    Ok(TargetSelection::RoundRobin)
                }
            }
        }
    }

    /// Validates the `X-OAGW-Target-Host` value format (hostname or IP, no
    /// port/path/special characters — `inst-dp-thx-format`).
    fn validate_target_host(header: &str) -> Result<(), DomainError> {
        validate_rfc1123_hostname(header).map_err(|e| DomainError::InvalidTargetHost {
            detail: e,
            host: Some(header.to_owned()),
        })
    }

    // ---------------------------------------------------------------------
    // Header matrix (algorithm `cpt-cf-oagw-algo-data-plane-proxy-headers`)
    // ---------------------------------------------------------------------

    /// Applies the inbound passthrough policy (`inst-dp-hdr-categorize`):
    /// `None` forwards nothing beyond the transform/plugin set, `Allowlist`
    /// forwards the listed names, `All` forwards everything.  Plugin-injected
    /// and transform-set headers are layered on later, so they always survive.
    fn apply_passthrough_policy(inbound: &Headers, cfg: &RequestHeadersConfig) -> Headers {
        let mut out = Headers::new();
        match cfg.passthrough {
            PassthroughMode::None => {}
            PassthroughMode::Allowlist => {
                for (name, value) in inbound.iter() {
                    if cfg
                        .passthrough_allowlist
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(name))
                    {
                        out.append(name, value);
                    }
                }
            }
            PassthroughMode::All => {
                for (name, value) in inbound.iter() {
                    out.append(name, value);
                }
            }
        }
        out
    }

    /// Applies the upstream request header transform operations (set, add,
    /// remove — `inst-dp-hdr-transform`).
    fn apply_request_transforms(headers: &mut Headers, cfg: &RequestHeadersConfig) {
        for name in &cfg.remove {
            headers.remove(name);
        }
        for (name, value) in &cfg.set {
            headers.insert(name, value);
        }
        for (name, value) in &cfg.add {
            headers.append(name, value);
        }
    }

    /// Applies the upstream response header transform operations (set, add,
    /// remove).
    fn apply_response_transforms(headers: &mut Headers, cfg: &ResponseHeadersConfig) {
        for name in &cfg.remove {
            headers.remove(name);
        }
        for (name, value) in &cfg.set {
            headers.insert(name, value);
        }
        for (name, value) in &cfg.add {
            headers.append(name, value);
        }
    }

    /// Strips the hop-by-hop headers (`Connection`, `Keep-Alive`,
    /// `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`,
    /// `Transfer-Encoding`, `Upgrade` — `inst-dp-hdr-strip`).
    fn strip_hop_by_hop(headers: &mut Headers) {
        for name in HOP_BY_HOP {
            headers.remove(name);
        }
    }

    /// Removes gateway routing/attribution headers from the outbound set
    /// (`inst-dp-hdr-osrc`).
    fn strip_routing_headers(headers: &mut Headers) {
        for name in ROUTING_HEADERS {
            headers.remove(name);
        }
    }

    /// Rejects outbound header names/values containing CR or LF (HTTP
    /// smuggling defense — `inst-dp-hdr-crlf`).
    fn check_crlf(headers: &Headers) -> Result<(), DomainError> {
        for (name, value) in headers.iter() {
            if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                return Err(DomainError::validation(
                    Some("headers"),
                    "outbound header name or value contains a CR/LF control character",
                ));
            }
        }
        Ok(())
    }

    /// Rewrites the `Host` header to the selected endpoint authority
    /// (`host[:port]` when non-standard — `inst-dp-hdr-host`).
    fn rewrite_host(headers: &mut Headers, endpoint: &Endpoint) {
        headers.insert("host", authority_host(endpoint));
    }

    /// Filters the request query against the route's query allowlist (an
    /// empty allowlist allows none — `inst-dp-hdr-query`).
    #[must_use]
    fn filter_query(query: &[(String, String)], allowlist: &[String]) -> Vec<(String, String)> {
        query
            .iter()
            .filter(|(name, _)| allowlist.iter().any(|a| a == name))
            .cloned()
            .collect()
    }

    /// Enforces the route's path-suffix mode (`inst-dp-hdr-query`):
    /// `Disabled` rejects any suffix beyond the route's `path_prefix`;
    /// `Append` forwards the full requested path (the default).
    fn apply_path_suffix(rest_path: &str, matched: &HttpMatch) -> Result<String, DomainError> {
        let suffix = match rest_path.strip_prefix(matched.path_prefix.as_str()) {
            None => return Ok(rest_path.to_owned()),
            Some(s) => s,
        };
        match matched.path_suffix_mode {
            PathSuffixMode::Append => Ok(rest_path.to_owned()),
            PathSuffixMode::Disabled => {
                if suffix.is_empty() {
                    Ok(matched.path_prefix.clone())
                } else {
                    Err(DomainError::validation(
                        Some("path_suffix_mode"),
                        format!(
                            "path suffix '{}' is not allowed for route prefix '{}' \
                             (path_suffix_mode=disabled)",
                            suffix, matched.path_prefix
                        ),
                    ))
                }
            }
        }
    }

    /// Validates the inbound framing headers (`inst-dp-ssrf-te`): a request
    /// carrying both `Content-Length` and `Transfer-Encoding`, or an invalid
    /// `Content-Length`, is rejected.  Returns the declared body length for
    /// the 100 MB pre-check (`inst-dp-ssrf-body`).
    fn check_framing(headers: &Headers) -> Result<Option<u64>, DomainError> {
        let has_te = headers.contains("transfer-encoding");
        let content_length = match headers.get("content-length") {
            Some(raw) => Some(raw.parse::<u64>().map_err(|_| {
                DomainError::validation(
                    Some("content-length"),
                    format!("invalid Content-Length value '{raw}'"),
                )
            })?),
            None => None,
        };
        if has_te && content_length.is_some() {
            return Err(DomainError::validation(
                Some("headers"),
                "request carries both Content-Length and Transfer-Encoding",
            ));
        }
        Ok(content_length)
    }

    // ---------------------------------------------------------------------
    // Effective rate limiting (algorithm `cpt-cf-oagw-algo-rate-limiting-effective-min`)
    // ---------------------------------------------------------------------

    /// Merges the upstream-effective and route rate-limit configs keeping the
    /// stricter (minimal) sustained rate.  Mirrors the private
    /// [`merge_rate_limits`](crate::domain::merge) semantics local to the
    /// Data Plane (the domain helper is module-private); the route cannot
    /// loosen an upstream limit.
    #[must_use]
    fn effective_rate_limit(
        base: Option<&RateLimitConfig>,
        route: Option<&RateLimitConfig>,
    ) -> Option<RateLimitConfig> {
        match (base, route) {
            (None, None) => None,
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (Some(a), Some(b)) => Some(merge_rate_limits(a.clone(), b)),
        }
    }

    /// Merges the tenant-chain effective CORS (already enforce-resolved within
    /// the hierarchy) with the matched route's CORS override
    /// (`inst-dp-cfg-base` / `inst-dp-cfg-route` / `inst-dp-cfg-enforce`).
    ///
    /// - The route cannot *replace* the tenant-effective set: a route CORS is
    ///   an overlay, mirroring the domain merge semantics.
    /// - When the effective base is enforce-resolved, the route may only
    ///   **tighten** it (`inst-dp-cfg-enforce`): origins/methods/exposed
    ///   headers are intersected and `allow_credentials`/`enabled` cannot
    ///   widen beyond the enforced set.
    /// - Otherwise the route set is unioned onto the base (inherit semantics,
    ///   [`merge_cors_union`]).
    #[must_use]
    fn effective_cors(base: Option<&CorsConfig>, route: Option<&CorsConfig>) -> Option<CorsConfig> {
        match (base, route) {
            (Some(b), None) => Some(b.clone()),
            (None, Some(r)) => Some(r.clone()),
            (Some(b), Some(r)) => {
                if b.sharing == SharingMode::Enforce {
                    Some(tighten_cors(b, r))
                } else {
                    Some(merge_cors_union(b.clone(), r))
                }
            }
            (None, None) => None,
        }
    }

    /// Runs the rate-limit check against the effective bucket
    /// (`inst-dp-exec-rl`).  `None` effective config → unlimited.
    ///
    /// The returned `Err` carries the 429 `rate_limit.exceeded` gateway error
    /// plus the `X-RateLimit-*`/`Retry-After` projections.  `Queue`/`Degrade`
    /// strategies terminate as a 429-style rejection in this delivery — an
    /// **explicit deferral**: FEATURE flow
    /// `cpt-cf-oagw-flow-rate-limiting-queue-degrade` and DoD
    /// `cpt-cf-oagw-dod-rate-limiting-strategies` (both `p4`, unchecked)
    /// describe the bounded admission queue and the degrade fallback, and the
    /// canonical DESIGN §4.7 lists backpressure queueing / graceful
    /// degradation under load as a future-development item.  Until then,
    /// fail-closed 429 is the interim behavior; the delivered contract is the
    /// per-instance `reject` strategy (`cpt-cf-oagw-flow-rate-limiting-reject`).
    async fn check_rate_limit(
        &self,
        leaf: &Upstream,
        route: &Route,
        config: Option<RateLimitConfig>,
    ) -> Result<Option<RateLimitInfo>, (DomainError, RateLimitInfo)> {
        let Some(config) = config else {
            return Ok(None);
        };
        let request = RateLimitRequest {
            tenant_id: leaf.tenant_id,
            upstream_id: leaf.id,
            route_id: Some(route.id),
            config,
        };
        let decision = self.rate_limiter.decide(&request).await;
        match decision {
            RateLimitDecision::Allow(info) => Ok(Some(info)),
            RateLimitDecision::Queue(info)
            | RateLimitDecision::Degrade(info)
            | RateLimitDecision::Rejected(info) => {
                let err = info.exceeded_error("rate limit exceeded");
                Err((err, info))
            }
        }
    }

    /// Observes the sustaining rate-limit usage ratio for `oagw_rate_limit_usage_ratio`
    /// (`inst-ob-rl-ratio`): the consumed share of the burst capacity, bounded
    /// [0.0, 1.0] (the canonical gauge bound — docs DESIGN §4.2; 1.0 = bucket
    /// exhausted).
    fn observe_rate_limit_usage(&self, host: &str, path: &str, info: &RateLimitInfo) {
        let ratio = if info.limit > 0 {
            (info.limit.saturating_sub(info.remaining)) as f64 / info.limit as f64
        } else {
            0.0
        };
        self.metrics.observe_rate_limit_usage(host, path, ratio);
    }

    // ---------------------------------------------------------------------
    // Effective plugin chain
    // ---------------------------------------------------------------------

    /// Assembles the effective plugin binding list: the merged (tenant-chain)
    /// auth binding first (synthetic, position 0), then the base upstream
    /// plugins with the route plugins layered last (ancestor first — algorithm
    /// `cpt-cf-oagw-algo-data-plane-proxy-apply-config`).  Positions are
    /// re-derived contiguous from 0.
    #[must_use]
    fn effective_bindings(merged: &UpstreamConfig, route: &Route) -> Vec<PluginBinding> {
        let mut bindings: Vec<PluginBinding> = Vec::new();
        let mut position: u32 = 0;
        if let Some(auth) = &merged.auth
            && let Some(plugin_type) = &auth.plugin_type
        {
            bindings.push(PluginBinding {
                position,
                plugin_ref: plugin_type.clone(),
                plugin_uuid: None,
                config: auth.config.clone(),
            });
            position += 1;
        }
        let combined = route.plugins.concat_ancestor(&merged.plugins);
        for mut binding in combined.items {
            binding.position = position;
            position += 1;
            bindings.push(binding);
        }
        bindings
    }

    // ---------------------------------------------------------------------
    // Forwarding + response passthrough
    // ---------------------------------------------------------------------

    /// Forwards the transformed request over hyper with the
    /// `proxy_timeout_secs` request timeout and **no retry** (single attempt —
    /// `inst-dp-exec-timeout`, `inst-dp-pass-noretry`).  Returns the raw
    /// upstream response (status/headers/streaming body untouched by the
    /// gateway beyond the transport).
    async fn forward(
        &self,
        hyper_request: HyperRequest<CapBody>,
        endpoint: &Endpoint,
        outbound_path: &str,
        leaf: &Upstream,
    ) -> Result<Response<HyperIncoming>, ProxyFailure> {
        let target = TargetContext {
            upstream_id: leaf.id,
            host: authority_host(endpoint),
            path: outbound_path.to_owned(),
        };

        // SSRF re-check (inst-dp-ssrf-scheme / inst-dp-ssrf-host): the
        // selected endpoint must still pass the scheme allowlist + RFC 1123
        // host rules under the current policy before the socket opens.
        let ssrf_target = target.clone();
        validate_endpoint_url(endpoint, self.cfg.allow_http_upstream).map_err(|e| {
            ProxyFailure::gateway_with_target(
                DomainError::LinkUnavailable {
                    detail: format!("endpoint rejected by the SSRF guard: {e}"),
                },
                ssrf_target,
            )
        })?;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.cfg.proxy_timeout_secs),
            self.hyper.request(hyper_request),
        )
        .await;

        match result {
            Ok(Ok(response)) => {
                // The upstream answered — availability 1 for `oagw_upstream_available`
                // (algorithm `cpt-cf-oagw-algo-observability-audit-record-routing`,
                // `inst-ob-ru-up`).
                self.metrics
                    .record_upstream_available(&leaf.alias, &endpoint.host, true);
                Ok(response)
            }
            Ok(Err(err)) => {
                let connect_failed = err.is_connect();
                let failure = Self::map_hyper_error(&err, &target);
                if connect_failed {
                    self.metrics
                        .record_upstream_available(&leaf.alias, &endpoint.host, false);
                }
                Err(failure)
            }
            Err(_elapsed) => {
                self.metrics
                    .record_upstream_available(&leaf.alias, &endpoint.host, false);
                Err(ProxyFailure::gateway_with_target(
                    DomainError::RequestTimeout {
                        detail: format!(
                            "upstream request exceeded proxy_timeout_secs={}",
                            self.cfg.proxy_timeout_secs
                        ),
                    },
                    target,
                ))
            }
        }
    }

    /// Maps a hyper transport failure onto the DESIGN error catalog
    /// (ADR 0007 — all gateway-intrinsic).
    fn map_hyper_error(
        err: &hyper_util::client::legacy::Error,
        target: &TargetContext,
    ) -> ProxyFailure {
        // A body-cap overflow surfaces through the hyper error source chain
        // (our streaming body errors inside the send task).
        if find_in_sources(err, |e| e.is::<BodyTooLarge>()) {
            return ProxyFailure::gateway_with_target(
                DomainError::PayloadTooLarge {
                    max_bytes: Some(MAX_BODY_BYTES),
                },
                target.clone(),
            );
        }
        if err.is_connect() {
            return ProxyFailure::gateway_with_target(
                DomainError::LinkUnavailable {
                    detail: err.to_string(),
                },
                target.clone(),
            );
        }
        ProxyFailure::gateway_with_target(
            DomainError::ProtocolError {
                detail: err.to_string(),
                cause: None,
            },
            target.clone(),
        )
    }

    /// Builds the streaming passthrough response: strips hop-by-hop headers,
    /// applies the response header transforms, runs the response plugin chain
    /// (guards + transforms), adds the CORS and `X-OAGW-Error-Source: upstream`
    /// headers, and streams the body (`inst-dp-exec-stream`).  Upstream
    /// 4xx/5xx statuses pass through unchanged (ADR 0007 — never wrapped in a
    /// gateway envelope).
    async fn build_passthrough(
        &self,
        upstream: Response<HyperIncoming>,
        leaf: &Upstream,
        chain: &PluginChain,
        cors_headers: Option<CorsHeaders>,
    ) -> Result<Response<AxumBody>, ProxyFailure> {
        let status = upstream.status();
        let upstream_id = leaf.id;
        let upstream_host = leaf
            .server
            .endpoints
            .first()
            .map(authority_host)
            .unwrap_or_else(|| leaf.alias.clone());

        let mut response_headers = headers_from_http(upstream.headers());
        Self::strip_hop_by_hop(&mut response_headers);
        Self::apply_response_transforms(&mut response_headers, &leaf.headers.response);

        let mut rctx = ResponseContext {
            status: status.as_u16(),
            headers: response_headers,
            config: serde_json::Value::Null,
        };
        match chain.run_response_chain(&mut rctx).await {
            ResponseOutcome::Approved => {}
            ResponseOutcome::GuardRejected(rejection) => {
                let mut ec = ErrorContext {
                    status: status.as_u16(),
                    headers: rctx.headers.clone(),
                    detail: format!("{}: {}", rejection.code, rejection.detail),
                    config: serde_json::Value::Null,
                };
                let _ = chain.transform_error(&mut ec).await;
                return Err(ProxyFailure::gateway_with_target(
                    rejection.to_domain_error(),
                    TargetContext {
                        upstream_id,
                        host: upstream_host,
                        path: String::new(),
                    },
                ));
            }
            ResponseOutcome::Failed(err) => {
                let mut ec = ErrorContext {
                    status: status.as_u16(),
                    headers: rctx.headers.clone(),
                    detail: err.to_string(),
                    config: serde_json::Value::Null,
                };
                let _ = chain.transform_error(&mut ec).await;
                return Err(ProxyFailure::gateway_with_target(
                    err,
                    TargetContext {
                        upstream_id,
                        host: upstream_host,
                        path: String::new(),
                    },
                ));
            }
        }

        // Assemble the final response header set: the (transformed) upstream
        // headers plus the CORS actual headers, then the `upstream` source
        // attribution.
        let mut final_headers = Headers::new();
        for (name, value) in rctx.headers.iter() {
            final_headers.append(name, value);
        }
        if let Some(cors) = cors_headers {
            for (name, value) in cors.as_header_pairs() {
                final_headers.append(&name, &value);
            }
        }
        final_headers.insert(
            crate::infra::error_envelope::ERROR_SOURCE_HEADER,
            crate::domain::error::ErrorSource::Upstream.as_str(),
        );

        // SSE-compatible passthrough: the body streams chunk-by-chunk, never
        // buffered by the gateway.
        let body = AxumBody::new(upstream.into_body());
        let mut response = Response::new(body);
        *response.status_mut() = status;
        append_headers(&mut response, &final_headers).map_err(ProxyFailure::gateway)?;
        Ok(response)
    }

    // ---------------------------------------------------------------------
    // The proxy hot path
    // ---------------------------------------------------------------------

    /// Executes the full data-plane proxy request (flow
    /// `cpt-cf-oagw-flow-data-plane-proxy-execute`).  The handler extracts
    /// the [`SecurityContext`] and the parsed proxy surface; its renderer
    /// turns the returned [`ProxyFailure`] into the RFC 9457 envelope (with
    /// the `X-RateLimit-*`/CORS decorations) or serves the streaming
    /// passthrough response unchanged.
    ///
    /// # Errors
    /// Every failure is a [`ProxyFailure`] whose `error` is a DESIGN catalog
    /// instance (ADR 0007 `gateway` attribution): 400 routing/validation,
    /// 401/403 authorization, 404 route-not-found, 413 payload-too-large,
    /// 429 rate-limit, 500/502/503/504 gateway/upstream-link errors, or the
    /// raw 403 CORS rejection.
    /// Proxies one request to its target alias (flow
    /// `cpt-cf-oagw-flow-data-plane-proxy-execute`) and records the
    /// per-request observability series (algorithm
    /// `cpt-cf-oagw-algo-observability-audit-record-request`):
    ///
    /// - `oagw_requests_in_flight` in/out via an [`InFlightGuard`]
    ///   (`inst-ob-rr-inflight`);
    /// - `oagw_requests_total{host, method, http.route, status}` and
    ///   `oagw_request_duration_seconds{host, http.route, phase="total"}`
    ///   (`inst-ob-rr-requests`, `inst-ob-rr-duration`);
    /// - `oagw_errors_total{host, http.route, error_type, error_source}`
    ///   labeled by the GTS instance for gateway failures and the bounded
    ///   `upstream_http_error` + `upstream` source for upstream-produced
    ///   5xx (DoD `cpt-cf-oagw-dod-observability-audit-error-source`,
    ///   ADR 0007) — `inst-ob-rr-errors`.
    ///
    /// The recording is a bounded label-set lookup plus atomics after the
    /// request completes — no I/O and nothing on the request/response wire
    /// (§62 acceptance: observability never blocks or slows the proxy hot
    /// path).
    pub async fn proxy(
        &self,
        ctx: &SecurityContext,
        request: ProxyRequest,
    ) -> Result<Response<AxumBody>, ProxyFailure> {
        let started = Instant::now();
        let method = request.method.clone();
        let host = request.alias.clone();
        let _in_flight = InFlightGuard::enter(&self.metrics, host.clone());

        let (result, route) = self.proxy_stage(ctx, request).await;
        let route = route.as_deref().unwrap_or("unmatched");
        let status = match &result {
            Ok(response) => response.status().as_u16(),
            Err(failure) => failure.error.status(),
        };
        self.metrics.record_request(&host, &method, route, status);
        self.metrics
            .observe_duration(&host, route, "total", started.elapsed().as_secs_f64());
        match &result {
            Ok(response) if response.status().as_u16() >= 500 => {
                // Upstream-produced 5xx pass through untouched (ADR 0007);
                // attribute the failure to the upstream.
                self.metrics.record_error(
                    &host,
                    route,
                    "upstream_http_error",
                    ErrorSource::Upstream.as_str(),
                );
            }
            Err(failure) => {
                self.metrics.record_error(
                    &host,
                    route,
                    failure.error.instance(),
                    ErrorSource::Gateway.as_str(),
                );
            }
            _ => {}
        }
        result
    }

    /// The instrumented stage: runs [`Self::proxy_pipeline`] and returns its
    /// result together with the matched route pattern (the `http.route`
    /// metric label — `None` until step 4, so early failures label as
    /// `unmatched`).
    async fn proxy_stage(
        &self,
        ctx: &SecurityContext,
        request: ProxyRequest,
    ) -> (Result<Response<AxumBody>, ProxyFailure>, Option<String>) {
        let mut route_label: Option<String> = None;
        let result = self.proxy_pipeline(ctx, request, &mut route_label).await;
        (result, route_label)
    }

    /// The uninstrumented request pipeline (steps 1-13 of the DESIGN proxy
    /// flow), writing the matched route pattern into `route_label` for the
    /// metrics wrapper (the `http.route` metric label).
    async fn proxy_pipeline(
        &self,
        ctx: &SecurityContext,
        request: ProxyRequest,
        route_label: &mut Option<String>,
    ) -> Result<Response<AxumBody>, ProxyFailure> {
        // 1. Authorization (`inst-dp-exec-authz`).
        self.authorize_invoke(ctx)
            .await
            .map_err(ProxyFailure::gateway)?;

        // 2. Framing validation + the 100 MB Content-Length pre-check
        //    (`inst-dp-ssrf-te`, `inst-dp-ssrf-body`).
        let declared_length = match Self::check_framing(&request.headers) {
            Ok(len) => len,
            Err(err) => return Err(ProxyFailure::gateway(err)),
        };
        if let Some(len) = declared_length
            && len > MAX_BODY_BYTES
        {
            return Err(ProxyFailure::gateway(DomainError::PayloadTooLarge {
                max_bytes: Some(MAX_BODY_BYTES),
            }));
        }

        // 3. Resolve the tenant chain (subject → root) and the alias
        //    (`inst-dp-exec-resolve`).
        let chain = self
            .tenant_resolver
            .get_ancestors(
                ctx,
                TenantId(ctx.subject_tenant_id()),
                &GetAncestorsOptions::default(),
            )
            .await
            .map_err(|e| {
                ProxyFailure::gateway(DomainError::ServiceUnavailable {
                    detail: format!("tenant resolution failed: {e}"),
                    retry_after: None,
                    cause: None,
                })
            })?;

        let mut chain_ids: Vec<TenantId> = chain.ancestors.iter().map(|t| t.id).collect();
        chain_ids.insert(0, TenantId(ctx.subject_tenant_id()));

        let (merged, leaf, _chain_owned) = self
            .resolve_alias_chain(ctx, &request.alias, &chain_ids)
            .await
            .map_err(ProxyFailure::gateway)?;

        // 4. Route matching (`inst-dp-exec-match`) → 404 `route.not_found`.
        let (route, http_match) = self
            .match_route(&leaf, &request.method, &request.rest_path)
            .await
            .map_err(ProxyFailure::gateway)?;
        *route_label = Some(http_match.path_prefix.clone());

        // 5. Effective config: the tenant-chain base is already merged; layer
        //    the route overrides (`inst-dp-exec-merge`).
        let effective_rate =
            Self::effective_rate_limit(merged.rate_limit.as_ref(), route.rate_limit.as_ref());
        // The route CORS overlays the tenant-effective base per sharing mode
        // (union under inherit, tighten-only under enforce) — it must not
        // replace the merged ancestor/tenant set (`inst-dp-cfg-enforce`).
        let effective_cors = Self::effective_cors(merged.cors.as_ref(), route.cors.as_ref());
        let bindings = Self::effective_bindings(&merged, &route);

        // 6. Rate-limit check (`inst-dp-exec-rl`) — post-merge: the bucket is
        //    keyed on the resolved upstream/route (the FEATURE lists it at
        //    step 3, but the effective config is only known after match, so
        //    the check necessarily lands after resolution).  Rejections
        //    increment `oagw_rate_limit_exceeded_total` and the sustaining
        //    ratio is observed in `oagw_rate_limit_usage_ratio` on both the
        //    allowed and rejected paths (algorithm
        //    `cpt-cf-oagw-algo-observability-audit-record-rate-limit`,
        //    `inst-ob-rl-exceeded` / `inst-ob-rl-ratio`).
        match self.check_rate_limit(&leaf, &route, effective_rate).await {
            Ok(Some(info)) => self.observe_rate_limit_usage(&leaf.alias, &request.rest_path, &info),
            Ok(None) => {}
            Err((err, info)) => {
                self.metrics
                    .record_rate_limit_exceeded(&leaf.alias, &request.rest_path);
                self.observe_rate_limit_usage(&leaf.alias, &request.rest_path, &info);
                return Err(ProxyFailure::gateway(err).with_rate_limit(info));
            }
        }

        // 7. CORS actual-request enforcement (`inst-dp-exec-cors`).
        let origin = request.headers.get("origin").map(ToOwned::to_owned);
        let mut cors_headers: Option<CorsHeaders> = None;
        if let Some(cors_config) = &effective_cors {
            match evaluate_actual(&request.method, origin.as_deref(), cors_config) {
                CorsOutcome::NotEnabled => {}
                CorsOutcome::Allowed(headers) => {
                    cors_headers = Some(headers);
                }
                CorsOutcome::Violation(violation) => {
                    return Err(ProxyFailure::cors(violation));
                }
            }
        }

        // 8. Plugin chain build + request half (auth → guards →
        //    transform(request) — `inst-dp-exec-plugins`).
        let custom = RepoCustomLookup {
            repo: self.plugins.as_ref(),
        };
        let chain = PluginChain::build(&bindings, &self.registries, leaf.tenant_id, Some(&custom))
            .await
            .map_err(|e| ProxyFailure::gateway(e.to_domain_error()))?;

        let mut rctx_in = RequestContext {
            method: request.method.clone(),
            path: request.rest_path.clone(),
            query: request.query.clone(),
            headers: request.headers.clone(),
            config: serde_json::Value::Null,
            security: Some(ctx.clone()),
        };
        match chain.run_request_chain(&mut rctx_in).await {
            ChainOutcome::Proceed => {}
            ChainOutcome::Rejected(rejection) => {
                let mut ec = ErrorContext {
                    status: rejection.status,
                    headers: rctx_in.headers.clone(),
                    detail: format!("{}: {}", rejection.code, rejection.detail),
                    config: serde_json::Value::Null,
                };
                let _ = chain.transform_error(&mut ec).await;
                return Err(ProxyFailure::gateway(rejection.to_domain_error()));
            }
            ChainOutcome::Failed(err) => {
                let mut ec = ErrorContext {
                    status: 400,
                    headers: rctx_in.headers.clone(),
                    detail: err.to_string(),
                    config: serde_json::Value::Null,
                };
                let _ = chain.transform_error(&mut ec).await;
                return Err(ProxyFailure::gateway(err));
            }
        }

        // 9. Target-host selection (`inst-dp-thx-*`), recording the routing
        //    series (algorithm
        //    `cpt-cf-oagw-algo-observability-audit-record-routing`): the
        //    explicit-header path increments `oagw_routing_target_host_used`,
        //    and every selection lands in `oagw_routing_endpoint_selected`
        //    with `explicit_header` / `round_robin` / `default`
        //    (`inst-ob-ru-target`, `inst-ob-ru-endpoint`).
        let target_header = request
            .headers
            .get(TARGET_HOST_HEADER)
            .map(ToOwned::to_owned);
        let endpoints = leaf.server.endpoints.as_slice();
        let (endpoint, selection_method) =
            match Self::select_endpoint(endpoints, &leaf.alias, target_header.as_deref())
                .map_err(ProxyFailure::gateway)?
            {
                TargetSelection::Endpoint(ep) => {
                    let method = if target_header.is_some() {
                        self.metrics.record_target_host_used(&leaf.id, &ep.host);
                        "explicit_header"
                    } else {
                        "default"
                    };
                    (ep, method)
                }
                TargetSelection::RoundRobin => {
                    let idx = self.rr.fetch_add(1, Ordering::Relaxed) % endpoints.len();
                    (&endpoints[idx], "round_robin")
                }
            };
        self.metrics
            .record_endpoint_selected(&leaf.id, &endpoint.host, selection_method);

        // 10. Header matrix (passthrough policy → transforms → plugin overlay
        //     → hop-by-hop/routing strip → CRLF guard → Host rewrite).
        let mut outbound = Self::apply_passthrough_policy(&request.headers, &leaf.headers.request);
        Self::apply_request_transforms(&mut outbound, &leaf.headers.request);
        // Plugin-injected headers (auth credential injection, request-id
        // propagation) survive: re-layer only headers the chain actually
        // *added* — names that were not part of the inbound request and that
        // no `remove` transform excluded.  The client-supplied inbound set is
        // never re-layered: the passthrough policy (`None`/`Allowlist`) and the
        // `remove` transforms have already decided its fate, and re-adding it
        // here would leak client credentials to the upstream against the
        // operator's header-minimization policy (`inst-dp-hdr-categorize`,
        // `inst-dp-hdr-transform`).
        let inbound_names = request.headers.names();
        for (name, value) in rctx_in.headers.iter() {
            if inbound_names.contains(name) {
                // Client-supplied: the matrix already handled it.
                continue;
            }
            if leaf
                .headers
                .request
                .remove
                .iter()
                .any(|r| r.eq_ignore_ascii_case(name))
            {
                // Explicitly removed by a transform.
                continue;
            }
            if outbound.get(name).is_none() {
                outbound.append(name, value);
            }
        }
        Self::strip_hop_by_hop(&mut outbound);
        Self::strip_routing_headers(&mut outbound);
        Self::check_crlf(&outbound).map_err(ProxyFailure::gateway)?;
        Self::rewrite_host(&mut outbound, endpoint);

        // 11. Path suffix mode + query allowlist (`inst-dp-hdr-query`).
        let outbound_path = Self::apply_path_suffix(&request.rest_path, &http_match)
            .map_err(ProxyFailure::gateway)?;
        let query = Self::filter_query(&request.query, &http_match.query_allowlist);
        let query_string = build_query_string(&query);

        // 12. Build and forward the hyper request (streaming body, no retry).
        let body = CapBody {
            inner: request.body,
            remaining: MAX_BODY_BYTES,
            declared_length,
        };
        let uri = build_uri(endpoint, &outbound_path, query_string.as_deref())
            .map_err(ProxyFailure::gateway)?;
        let headers_map = headers_to_http(&outbound).map_err(ProxyFailure::gateway)?;

        let mut builder = HyperRequest::builder()
            .method(request.method.as_str())
            .uri(uri);
        for (name, value) in headers_map.iter() {
            builder = builder.header(name.clone(), value.clone());
        }
        let hyper_request = builder
            .body(body)
            .map_err(|e| ProxyFailure::gateway(DomainError::validation(None, e.to_string())))?;

        tracing::trace!(
            upstream_id = %leaf.id,
            host = %endpoint.host,
            outbound_path = %outbound_path,
            method = %request.method,
            "proxy request forwarded"
        );
        let upstream_response = self
            .forward(hyper_request, endpoint, &outbound_path, &leaf)
            .await?;

        // 13. Response passthrough + response plugin chain + CORS/source
        //     headers (`inst-dp-exec-stream`, `inst-dp-exec-return`).
        self.build_passthrough(upstream_response, &leaf, &chain, cors_headers)
            .await
    }
}

/// The outcome of `X-OAGW-Target-Host` selection (algorithm
/// `cpt-cf-oagw-algo-data-plane-proxy-target-host`).
#[derive(Debug)]
enum TargetSelection<'a> {
    /// A concrete endpoint was selected by the matrix.
    Endpoint(&'a Endpoint),
    /// Explicit-alias multi-endpoint pool with no header — round-robin.
    RoundRobin,
}

/// Streaming request-body wrapper enforcing the 100 MB cap on the wire
/// (`inst-dp-ssrf-body`).  When the cap is exceeded the body surface errors
/// with a [`BodyTooLarge`] marker that [`find_in_sources`] detects after the
/// hyper call so the 413 `payload.too_large` is served.
struct CapBody {
    inner: AxumBody,
    remaining: u64,
    /// The inbound `Content-Length` (already validated ≤ cap by the
    /// pre-check); used to size-hint the stream so hyper can frame the
    /// request with `Content-Length` when known.
    declared_length: Option<u64>,
}

/// Marker error for "request body exceeded the 100 MB cap".
#[derive(Debug)]
struct BodyTooLarge;

impl std::fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body exceeds the 100 MB cap")
    }
}

impl std::error::Error for BodyTooLarge {}

impl HttpBody for CapBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let frame = match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(err.into()))),
            Poll::Ready(Some(Ok(frame))) => frame,
        };
        if let Some(data) = frame.data_ref() {
            let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
            if len > self.remaining {
                return Poll::Ready(Some(Err(Box::new(BodyTooLarge))));
            }
            self.remaining -= len;
        }
        Poll::Ready(Some(Ok(frame)))
    }

    fn size_hint(&self) -> SizeHint {
        match self.declared_length {
            Some(len) => SizeHint::with_exact(len),
            None => self.inner.size_hint(),
        }
    }
}

/// Adapts a [`PluginRepository`] handle into the narrow [`CustomPluginLookup`]
/// bound the chain builder expects.  The registries blanket impl covers every
/// `Sized` `PluginRepository`; our handle is a `dyn` object (unsized), so the
/// adapter forwards the single `get_custom` query explicitly.
struct RepoCustomLookup<'a> {
    repo: &'a dyn PluginRepository,
}

#[async_trait]
impl CustomPluginLookup for RepoCustomLookup<'_> {
    async fn get_custom(&self, tenant_id: Uuid, id: Uuid) -> Option<crate::domain::entity::Plugin> {
        PluginRepository::get(self.repo, tenant_id, id).await
    }
}

/// Walks an error's source chain looking for a downcastable entry.
fn find_in_sources<E: std::error::Error + 'static>(
    err: &E,
    mut predicate: impl FnMut(&(dyn std::error::Error + 'static)) -> bool,
) -> bool {
    let mut current: &(dyn std::error::Error + 'static) = err;
    loop {
        if predicate(current) {
            return true;
        }
        match current.source() {
            Some(next) => current = next,
            None => return false,
        }
    }
}

/// Whether an endpoint host matches the `X-OAGW-Target-Host` value
/// (case-insensitive, trailing FQDN dot tolerated).
fn endpoint_has_host(endpoint: &Endpoint, header: &str) -> bool {
    endpoint
        .host
        .trim_end_matches('.')
        .eq_ignore_ascii_case(header.trim_end_matches('.'))
}

/// The valid host names of a pool, for the routing 400 details.
fn known_hosts(endpoints: &[Endpoint]) -> String {
    endpoints
        .iter()
        .map(|e| e.host.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The authority `host[:port]` to dial for an endpoint (port included only
/// when non-standard; IPv6 literals bracketed).
fn authority_host(endpoint: &Endpoint) -> String {
    let standard = standard_port(endpoint.scheme);
    let host = if endpoint.host.contains(':') && !endpoint.host.starts_with('[') {
        format!("[{}]", endpoint.host)
    } else {
        endpoint.host.clone()
    };
    if standard == Some(endpoint.port) {
        host
    } else {
        format!("{host}:{}", endpoint.port)
    }
}

/// Builds the outbound `http::Uri` for the selected endpoint.
fn build_uri(endpoint: &Endpoint, path: &str, query: Option<&str>) -> Result<Uri, DomainError> {
    let scheme = match endpoint.scheme {
        EndpointScheme::Https => "https",
        EndpointScheme::Http => "http",
        // WSS/WebTransport/gRPC endpoints are not reachable over the plain
        // HTTP transport (gRPC is Phase 3; WSS/WT need a dedicated transport).
        other => {
            return Err(DomainError::LinkUnavailable {
                detail: format!(
                    "endpoint scheme '{other:?}' is not supported by the HTTP proxy transport"
                ),
            });
        }
    };
    let authority = authority_host(endpoint);
    let path_and_query = match (query, path) {
        (Some(q), _) if !q.is_empty() => format!("{path}?{q}"),
        (_, p) => p.to_owned(),
    };
    format!("{scheme}://{authority}{path_and_query}")
        .parse::<Uri>()
        .map_err(|e| DomainError::validation(None, format!("invalid upstream URI: {e}")))
}

/// Rebuilds a raw query string from the (allowlisted) `(name, value)` pairs,
/// percent-encoding each part.
fn build_query_string(query: &[(String, String)]) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (name, value) in query {
        serializer.append_pair(name, value);
    }
    Some(serializer.finish())
}

/// Converts the domain [`Headers`] into an `http::HeaderMap`, rejecting names
/// or values that cannot be represented on the wire.
fn headers_to_http(headers: &Headers) -> Result<HeaderMap, DomainError> {
    let mut map = HeaderMap::new();
    for (name, value) in headers.iter() {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            DomainError::validation(None, format!("invalid header name '{name}': {e}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|e| {
            DomainError::validation(None, format!("invalid header value for '{name}': {e}"))
        })?;
        map.append(name, value);
    }
    Ok(map)
}

/// Converts an `http::HeaderMap` (an upstream response head) into the domain
/// [`Headers`].
fn headers_from_http(headers: &HeaderMap) -> Headers {
    let mut out = Headers::new();
    for (name, value) in headers {
        if let Ok(value_str) = value.to_str() {
            out.append(name.as_str(), value_str);
        }
    }
    out
}

/// Appends the domain header set onto an outbound response, replacing like
/// names.
fn append_headers(response: &mut Response<AxumBody>, headers: &Headers) -> Result<(), DomainError> {
    for (name, value) in headers.iter() {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            DomainError::validation(None, format!("invalid header name '{name}': {e}"))
        })?;
        let value = HeaderValue::from_str(value)
            .map_err(|e| DomainError::validation(None, format!("invalid header value: {e}")))?;
        if response.headers().contains_key(&name) {
            response.headers_mut().insert(name, value);
        } else {
            response.headers_mut().append(name, value);
        }
    }
    Ok(())
}

/// Mirrors the module-private `merge::merge_rate_limits`: keeps the stricter
/// (minimal) sustained rate, preserving the algorithm/scope/strategy/cost of
/// the stricter side.
#[must_use]
fn merge_rate_limits(acc: RateLimitConfig, next: &RateLimitConfig) -> RateLimitConfig {
    let acc_rate = acc.sustained.rate;
    let next_rate = next.sustained.rate;
    if next_rate < acc_rate {
        RateLimitConfig {
            sharing: acc.sharing,
            sustained: next.sustained,
            burst: merge_burst(acc.burst, next.burst),
            ..next.clone()
        }
    } else {
        RateLimitConfig {
            sustained: acc.sustained,
            burst: merge_burst(acc.burst, next.burst),
            ..acc
        }
    }
}

/// Merges two burst configs keeping the minimal capacity.
#[must_use]
fn merge_burst(
    acc: Option<crate::domain::entity::config::BurstConfig>,
    next: Option<crate::domain::entity::config::BurstConfig>,
) -> Option<crate::domain::entity::config::BurstConfig> {
    use crate::domain::entity::config::BurstConfig;
    match (acc, next) {
        (Some(a), Some(b)) => Some(BurstConfig {
            capacity: match (a.capacity, b.capacity) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (None, y) => y,
                (x, None) => x,
            },
        }),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Tightens an enforce-resolved CORS base with the route's own set
/// (`inst-dp-cfg-enforce`): the effective set is the *intersection* of the
/// enforced base and the route override — the route may only restrict, never
/// widen the enforced origins/methods/exposed headers, and it cannot widen
/// `allow_credentials`/`enabled`.
#[must_use]
fn tighten_cors(base: &CorsConfig, route: &CorsConfig) -> CorsConfig {
    let allowed_origins = base
        .allowed_origins
        .iter()
        .filter(|o| route.allowed_origins.contains(o))
        .cloned()
        .collect();
    let allowed_methods = base
        .allowed_methods
        .iter()
        .filter(|m| route.allowed_methods.contains(m))
        .cloned()
        .collect();
    let expose_headers = base
        .expose_headers
        .iter()
        .filter(|h| route.expose_headers.contains(h))
        .cloned()
        .collect();
    CorsConfig {
        sharing: SharingMode::Enforce,
        enabled: base.enabled && route.enabled,
        allowed_origins,
        allowed_methods,
        expose_headers,
        allow_credentials: base.allow_credentials && route.allow_credentials,
    }
}

#[cfg(test)]
mod tests;
