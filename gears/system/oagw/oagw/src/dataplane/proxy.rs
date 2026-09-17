// Created: 2026-09-04 by Constructor Tech
//! Data-plane proxy engine
//! (`docs/DESIGN.md` §3.2 "Proxy API", `docs/ADR/0001-request-routing.md`).
//!
//! One call runs the stage order of `docs/ADR/0001-request-routing.md` and
//! `docs/ADR/0002-plugin-system.md`:
//!
//! ```text
//! upstream/route resolution → CORS preflight short-circuit → endpoint
//! selection → egress scheme gate → query allowlist → body limit → guard
//! plugins → auth plugin → request transforms → rate limiting → upstream →
//! response guards → response transforms → client response
//! ```
//!
//! Gateway failures are rendered as RFC 9457 problem bodies tagged
//! `X-OAGW-Error-Source: gateway`; an upstream response — success or failure —
//! is passed through untouched and tagged `X-OAGW-Error-Source: upstream`
//! (`docs/ADR/0007-error-source-distinction.md`).

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::extract::Extension;
use axum::extract::FromRequestParts;
use axum::extract::connect_info::ConnectInfo;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use serde_json::Value;
use tokio::time::Instant;
use toolkit_canonical_errors::Problem;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::controlplane::service::ControlPlaneService;
use crate::dataplane::cors::{self, CorsRejection};
use crate::dataplane::headers::{self, REQUEST_ID_HEADER};
use crate::dataplane::plugins::{
    CredentialResolver, ErrorContext, GuardDecision, PluginRegistry, RequestContext,
    ResponseContext,
};
use crate::dataplane::ratelimit::{self, RateLimiter, ScopeIdentity};
use crate::dataplane::streaming::{self, InboundUpgrade};
use crate::domain::route::best_route_match;
use crate::domain::{
    AliasDerivation, EffectivePolicy, Endpoint, HttpMethod, PathSuffixMode, PluginKind, PluginRef,
    RateLimitStrategy, ResponseHeaderRules, Route, Upstream, derive_alias,
};
use crate::error::{
    ERROR_DOMAIN, ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, OagwError,
    ProblemExtensions,
};

// Compile-time proof that the data plane can be shared across the axum
// router: `Extension<Arc<DataPlane>>` requires `Arc<DataPlane>: Send + Sync`.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DataPlane>();
};

/// Gear-relative path of the proxy surface. The api-gateway nests gear paths
/// under its own prefix, so this is the path the gear serves verbatim —
/// there is no leading `/api` segment.
pub const PROXY_ROUTE_PATH: &str = "/oagw/v1/proxy/{*alias}";

/// Prefix of the proxy path, before the alias segment.
pub const PROXY_ALIAS_PREFIX: &str = "/oagw/v1/proxy/";

/// Probe reporting whether the gear is shutting down.
///
/// The gear wiring of a later phase clones the host cancellation token into a
/// closure; the data plane itself depends on no cancellation-token type.
pub type CancelProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// A [`CancelProbe`] that never reports a shutdown.
#[must_use]
pub fn never_cancelled() -> CancelProbe {
    Arc::new(|| false)
}

/// Request handed to [`DataPlane::execute`] by the axum handler.
#[derive(Debug)]
pub struct ProxyCall {
    /// Alias segment of the proxy path, verbatim.
    pub alias: String,
    /// Path suffix following the alias (`/` when there is none).
    pub suffix: String,
    /// Request method.
    pub method: Method,
    /// Original request path, used as the problem `instance`.
    pub path: String,
    /// Raw query string of the request.
    pub query: Option<String>,
    /// Request headers.
    pub headers: HeaderMap,
    /// Request body.
    pub body: Body,
    /// Tenant of the authenticated subject.
    pub tenant_id: Uuid,
    /// Authenticated subject of the request.
    pub subject_id: Uuid,
    /// Client IP of the caller, when the host exposes it.
    pub client_ip: Option<String>,
    /// Inbound half of a protocol upgrade, when the caller asked for one.
    pub upgrade: InboundUpgrade,
}

/// The `Sync` subset of a [`ProxyCall`], detached from the request body.
///
/// `axum::body::Body` is `Send` but not `Sync`, so a scope still borrowing the
/// whole call would make the handler future non-`Send`. The pipeline reads
/// only the descriptor fields, so they are moved into an owned record before
/// the first `.await`.
#[derive(Debug)]
struct ForwardedCall {
    /// Alias segment of the proxy path, verbatim.
    alias: String,
    /// Path suffix following the alias (`/` when there is none).
    suffix: String,
    /// Request method.
    method: Method,
    /// Original request path, used as the problem `instance`.
    path: String,
    /// Raw query string of the request.
    query: Option<String>,
    /// Request headers.
    headers: HeaderMap,
    /// Tenant of the authenticated subject.
    tenant_id: Uuid,
    /// Authenticated subject of the request.
    subject_id: Uuid,
    /// Client IP of the caller, when the host exposes it.
    client_ip: Option<String>,
    /// Inbound half of a protocol upgrade, when the caller asked for one.
    upgrade: InboundUpgrade,
}

impl ForwardedCall {
    /// Moves the descriptor fields out of `call`, leaving only its body.
    fn detach(call: &mut ProxyCall) -> Self {
        Self {
            alias: std::mem::take(&mut call.alias),
            suffix: std::mem::take(&mut call.suffix),
            method: call.method.clone(),
            path: std::mem::take(&mut call.path),
            query: call.query.take(),
            headers: std::mem::take(&mut call.headers),
            tenant_id: call.tenant_id,
            subject_id: call.subject_id,
            client_ip: call.client_ip.take(),
            upgrade: std::mem::take(&mut call.upgrade),
        }
    }
}

/// Gateway failure carrying its own documented GTS type identifier
/// (`docs/DESIGN.md` §3.3) because the frozen Phase-1 error model has no
/// variant for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocumentedError {
    /// GTS instance id of the error type.
    gts_type_id: &'static str,
    /// Human-readable problem `title`.
    title: &'static str,
    /// HTTP status.
    status: u16,
    /// Machine-readable code name.
    code: &'static str,
    /// RFC 9457 `detail`.
    detail: String,
}

/// `X-OAGW-Target-Host` rejection
/// (`docs/DESIGN.md` §3.3 "MissingTargetHost", "InvalidTargetHost",
/// "UnknownTargetHost").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetHostRejection {
    /// A multi-endpoint upstream with a common-suffix alias needs the header.
    Missing,
    /// The header value is not a bare hostname or IP address.
    Invalid {
        /// Rejected value.
        value: String,
    },
    /// The header value names no endpoint of the upstream.
    Unknown {
        /// Rejected value.
        value: String,
    },
}

impl From<TargetHostRejection> for DocumentedError {
    fn from(rejection: TargetHostRejection) -> Self {
        match rejection {
            TargetHostRejection::Missing => Self {
                gts_type_id: crate::dataplane::MISSING_TARGET_HOST_TYPE,
                title: "Missing Target Host",
                status: 400,
                code: "MissingTargetHost",
                detail: String::from(
                    "X-OAGW-Target-Host is required for a multi-endpoint upstream with a \
                     common-suffix alias",
                ),
            },
            TargetHostRejection::Invalid { value } => Self {
                gts_type_id: crate::dataplane::INVALID_TARGET_HOST_TYPE,
                title: "Invalid Target Host",
                status: 400,
                code: "InvalidTargetHost",
                detail: format!(
                    "'{value}' is not a valid X-OAGW-Target-Host value (expected a bare \
                     hostname or IP address)"
                ),
            },
            TargetHostRejection::Unknown { value } => Self {
                gts_type_id: crate::dataplane::UNKNOWN_TARGET_HOST_TYPE,
                title: "Unknown Target Host",
                status: 400,
                code: "UnknownTargetHost",
                detail: format!("'{value}' does not match any endpoint of the upstream"),
            },
        }
    }
}

impl From<CorsRejection> for DocumentedError {
    fn from(rejection: CorsRejection) -> Self {
        match rejection {
            CorsRejection::Origin { origin } => Self {
                gts_type_id: crate::dataplane::CORS_ORIGIN_NOT_ALLOWED_TYPE,
                title: "CORS Origin Not Allowed",
                status: 403,
                code: "CorsOriginNotAllowed",
                detail: format!("origin '{origin}' is not in the allowed origins list"),
            },
            CorsRejection::Method { method } => Self {
                gts_type_id: crate::dataplane::CORS_METHOD_NOT_ALLOWED_TYPE,
                title: "CORS Method Not Allowed",
                status: 403,
                code: "CorsMethodNotAllowed",
                detail: format!("method '{method}' is not in the allowed methods list"),
            },
        }
    }
}

/// Every way a proxied request can fail before or after the upstream call.
#[derive(Debug)]
pub(crate) enum Failure {
    /// A failure of the Phase-1 error model.
    Gateway(OagwError),
    /// A failure documented with its own GTS type identifier.
    Documented(DocumentedError),
}

impl From<OagwError> for Failure {
    fn from(error: OagwError) -> Self {
        Self::Gateway(error)
    }
}

impl From<TargetHostRejection> for Failure {
    fn from(rejection: TargetHostRejection) -> Self {
        Self::Documented(DocumentedError::from(rejection))
    }
}

impl From<CorsRejection> for Failure {
    fn from(rejection: CorsRejection) -> Self {
        Self::Documented(DocumentedError::from(rejection))
    }
}

/// Per-request state shared by the pipeline and the error renderer.
#[derive(Debug)]
struct Scope {
    /// The request being proxied, without its body.
    call: ForwardedCall,
    /// Resolved upstream, once resolution succeeded.
    upstream_id: Option<Uuid>,
    /// Authority of the selected endpoint.
    host: Option<String>,
    /// Matched route.
    route_id: Option<Uuid>,
    /// Plugin chain in execution order.
    plugins: Vec<PluginRef>,
    /// Plugin configuration slot.
    config: Value,
    /// Request identifier chosen by the `request_id` transform plugin.
    request_id: Option<String>,
    /// Rate-limit accounting of the request.
    rate_headers: HeaderMap,
}

impl Scope {
    /// Scope of a request whose body has already been detached.
    fn new(call: ForwardedCall) -> Scope {
        Scope {
            call,
            upstream_id: None,
            host: None,
            route_id: None,
            plugins: Vec::new(),
            config: Value::Null,
            request_id: None,
            rate_headers: HeaderMap::new(),
        }
    }

    /// Problem extension fields of a failure
    /// (`docs/DESIGN.md` §3.3 "Error Response Format").
    fn extensions(&self) -> ProblemExtensions {
        ProblemExtensions {
            instance: Some(self.call.path.clone()),
            upstream_id: self.upstream_id,
            host: self.host.clone(),
            path: Some(self.call.path.clone()),
            trace_id: self.request_id.clone(),
        }
    }

    /// Plugin configuration slot of the request.
    fn plugin_config(&self) -> Value {
        self.config.clone()
    }
}

/// The data plane of the OAGW gear.
///
/// It resolves a proxy request against the control plane, applies the plugin
/// chain, the rate limiter and CORS, and forwards the request to the selected
/// endpoint of the upstream pool.
pub struct DataPlane {
    control_plane: Arc<ControlPlaneService>,
    config: Arc<OagwConfig>,
    cancelled: CancelProbe,
    limiter: RateLimiter,
    registry: PluginRegistry,
    client: UpstreamClient,
    round_robin: AtomicU64,
}

impl std::fmt::Debug for DataPlane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DataPlane")
            .field("config", &self.config)
            .field("limiter", &self.limiter)
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl DataPlane {
    /// Assembles a data plane over the control plane and the gear
    /// configuration (`docs/DESIGN.md` §3.1).
    #[must_use]
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        config: Arc<OagwConfig>,
        cancelled: CancelProbe,
    ) -> Self {
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .build_http::<Body>();
        Self {
            control_plane,
            config,
            cancelled,
            limiter: RateLimiter::new(),
            registry: PluginRegistry::with_builtins(None),
            client,
            round_robin: AtomicU64::new(0),
        }
    }

    /// Replaces the plugin registry (for a host registering its own plugins).
    #[must_use]
    pub fn with_plugin_registry(mut self, registry: PluginRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// Installs the credential resolver backing `cred://` references.
    #[must_use]
    pub fn with_credential_resolver(mut self, resolver: Arc<dyn CredentialResolver>) -> Self {
        self.registry = PluginRegistry::with_builtins(Some(resolver));
        self
    }

    /// `true` when the host asked the gear to shut down.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        (self.cancelled)()
    }

    /// The control plane the data plane resolves against.
    #[must_use]
    pub const fn control_plane(&self) -> &Arc<ControlPlaneService> {
        &self.control_plane
    }

    /// The gear-level configuration in force.
    #[must_use]
    pub const fn config(&self) -> &Arc<OagwConfig> {
        &self.config
    }

    /// The rate limiter of the data plane.
    #[must_use]
    pub const fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    /// The plugin registry of the data plane.
    #[must_use]
    pub const fn registry(&self) -> &PluginRegistry {
        &self.registry
    }

    /// Runs one proxied request and renders the answer.
    pub async fn execute(&self, mut call: ProxyCall) -> axum::response::Response {
        let mut body = std::mem::replace(&mut call.body, Body::empty());
        let mut scope = Scope::new(ForwardedCall::detach(&mut call));
        match self.pipeline(&mut scope, &mut body).await {
            Ok(response) => response,
            Err(failure) => self.render_failure(failure, &scope).await,
        }
    }

    /// The pipeline of one proxied request.
    async fn pipeline(
        &self,
        scope: &mut Scope,
        body: &mut Body,
    ) -> Result<axum::response::Response, Failure> {
        let call = &scope.call;
        let method =
            HttpMethod::parse(call.method.as_str()).map_err(|_| OagwError::Validation {
                detail: format!("method '{}' cannot be proxied", call.method.as_str()),
            })?;

        // --- 1. upstream and route resolution -----------------------------
        let upstream = self
            .upstream_for(call)
            .ok_or_else(|| OagwError::UpstreamNotFound {
                alias: normalize_alias(&call.alias),
            })?;
        if !upstream.is_enabled() {
            return Err(OagwError::LinkUnavailable {
                detail: format!("upstream '{}' is disabled", upstream.alias),
            }
            .into());
        }
        scope.upstream_id = Some(upstream.id);

        let route =
            self.route_for(&upstream, call, method)
                .ok_or_else(|| OagwError::RouteNotFound {
                    detail: format!(
                        "no route of upstream '{}' matches '{}' with {}",
                        upstream.alias,
                        call.suffix,
                        method.as_str()
                    ),
                })?;
        scope.route_id = Some(route.id);
        let policy = crate::domain::resolve_policy(&self.config, &upstream, Some(&route));
        scope.plugins = policy.plugins.clone();
        scope.config = plugin_config(&policy);

        // --- 2. CORS preflight short-circuit ------------------------------
        // A preflight is answered locally without reading the CORS policy
        // further: the answer is permissive (`docs/ADR/0004-cors.md`).
        if enabled_cors(&policy).is_some() && cors::is_preflight(&call.method, &call.headers) {
            return Ok(self.preflight_response(call));
        }

        // --- 3. endpoint selection and egress gates -----------------------
        let endpoint = self.select_endpoint(&upstream, headers::target_host(&call.headers))?;
        scope.host = Some(endpoint.authority());
        if !policy.permits_plaintext_egress(endpoint.scheme()) {
            return Err(OagwError::LinkUnavailable {
                detail: format!(
                    "plaintext '{}' egress is disabled by allow_http_upstream",
                    endpoint.scheme()
                ),
            }
            .into());
        }
        if let Some(cors_config) = enabled_cors(&policy) {
            let origin = origin_of(&call.headers);
            cors::check_actual(cors_config, origin.as_deref(), &call.method)?;
        }

        // --- 4. match guards ----------------------------------------------
        if let Some(matched) = route.http_match()
            && matched.path_suffix_mode == PathSuffixMode::Disabled
            && call.suffix.len() > 1
        {
            return Err(OagwError::Validation {
                detail: format!(
                    "the route matches '{}' exactly, so the suffix '{}' is not forwarded",
                    matched.path, call.suffix
                ),
            }
            .into());
        }
        let allowlist = route
            .http_match()
            .map(|matched| matched.query_allowlist.clone())
            .unwrap_or_default();
        let query = headers::filter_query(call.query.as_deref(), &allowlist)?;

        // --- 5. body limit ------------------------------------------------
        // An upgrade request carries no body — its handshake is forwarded as
        // it arrived (`docs/PRD.md` §5.4) — so it is never buffered.
        let upgrade = streaming::is_upgrade_request(&call.headers);
        let body = if upgrade {
            Bytes::new()
        } else {
            read_body(body, self.config.max_body_bytes).await?
        };

        // --- 6. outbound request and plugin chain -------------------------
        // The plugin chain sees what the client sent: a guard validates the
        // request as it arrived and the transforms read the caller's headers.
        let forwarded_path = forwarded_path(&route, &call.suffix);
        let mut ctx = RequestContext {
            tenant_id: call.tenant_id,
            subject_id: call.subject_id,
            upstream_id: upstream.id,
            route_id: scope.route_id,
            method,
            path: forwarded_path,
            query,
            headers: call.headers.clone(),
            body: (!body.is_empty()).then_some(body),
            request_id: None,
            config: scope.config.clone(),
        };
        self.run_request_plugins(&scope.plugins, &mut ctx).await?;
        scope.request_id = ctx.request_id.clone();

        // The upstream headers are the client-supplied set under the upstream
        // header rules, with the headers the plugin chain injected or rewrote
        // on top and the propagated `X-Request-ID`.
        let request_rules = policy
            .headers
            .as_ref()
            .and_then(|rules| rules.request.as_ref());
        let mut outbound =
            headers::outbound_request_headers(&call.headers, request_rules, &endpoint.authority());
        for (name, value) in ctx.headers.iter() {
            if call.headers.get(name) != Some(value) && !headers::is_hop_by_hop(name.as_str()) {
                outbound.insert(name, value.clone());
            }
        }
        if let Some(request_id) = ctx.request_id.as_ref()
            && let Ok(value) = HeaderValue::from_str(request_id)
        {
            outbound.insert(REQUEST_ID_HEADER, value);
        }
        if upgrade {
            streaming::restore_upgrade_headers(&call.headers, &mut outbound);
        }
        ctx.headers = outbound;

        // --- 7. rate limiting, before the upstream call -------------------
        let effective_limit =
            ratelimit::effective_limit(policy.rate_limit.as_ref(), route.rate_limit.as_ref());
        if let Some(limit) = effective_limit.as_ref().filter(|limit| rejects(limit)) {
            let decision = self.limiter.check(
                &ratelimit::bucket_key(
                    limit,
                    upstream.id,
                    scope.route_id,
                    &ScopeIdentity {
                        tenant_id: call.tenant_id,
                        subject_id: call.subject_id,
                        client_ip: call.client_ip.clone(),
                    },
                ),
                limit,
                Instant::now(),
            );
            decision.write_headers(&mut scope.rate_headers);
            if !decision.allowed {
                return Err(Failure::Gateway(decision.error()));
            }
        }

        // --- 8. upstream call ---------------------------------------------
        let upstream_uri = build_uri(endpoint, &ctx.path, ctx.query.as_deref())?;
        let mut request = http::Request::builder()
            .method(call.method.clone())
            .uri(upstream_uri)
            .body(Body::from(ctx.body.clone().unwrap_or_default()))
            .map_err(|error| OagwError::Internal {
                detail: format!("the upstream request could not be built: {error}"),
            })?;
        *request.headers_mut() = ctx.headers.clone();
        if upgrade {
            return self.forward_upgrade(scope, request).await;
        }
        let response = send(&self.client, request, self.config.proxy_timeout()).await?;

        // --- 9. response pipeline -----------------------------------------
        let response_rules = policy
            .headers
            .as_ref()
            .and_then(|rules| rules.response.as_ref());
        Ok(self
            .render_upstream_response(response, scope, &policy, response_rules, ctx.request_id)
            .await)
    }

    // ------------------------------------------------------------ resolution

    /// The tenant upstream registered under the requested alias.
    fn upstream_for(&self, call: &ForwardedCall) -> Option<Upstream> {
        let wanted = normalize_alias(&call.alias);
        self.control_plane
            .list_upstreams(call.tenant_id)
            .into_iter()
            .find(|upstream| upstream.alias.as_str() == wanted)
    }

    /// The enabled route of `upstream` matching the request.
    fn route_for(
        &self,
        upstream: &Upstream,
        call: &ForwardedCall,
        method: HttpMethod,
    ) -> Option<Route> {
        let routes: Vec<Route> = self
            .control_plane
            .list_routes(call.tenant_id)
            .into_iter()
            .filter(|route| route.is_enabled() && route.upstream_id == upstream.id)
            .collect();
        // A preflight is not a real request and never carries a matched
        // method (`OPTIONS` is not a route-matching method), so any route of
        // the upstream on the path is a match for it
        // (`docs/ADR/0004-cors.md`).
        if cors::is_preflight_method(method) {
            return routes
                .into_iter()
                .find(|route| route.proxy_path(&call.suffix).is_some());
        }
        best_route_match(&routes, &call.suffix, method).cloned()
    }

    /// Picks the endpoint of the pool, honouring `X-OAGW-Target-Host`
    /// (`docs/ADR/0001-request-routing.md` "X-OAGW-Target-Host Behavior
    /// Matrix").
    fn select_endpoint<'a>(
        &self,
        upstream: &'a Upstream,
        target: Option<&str>,
    ) -> Result<&'a Endpoint, Failure> {
        let endpoints = upstream.server.endpoints();
        // A common-suffix alias names the pool, not one endpoint: the caller
        // has to pin the target.
        let pins_target = endpoints.len() > 1
            && matches!(
                derive_alias(endpoints),
                Ok(AliasDerivation::Derived(_)) | Err(_)
            );
        let Some(target) = target else {
            if pins_target {
                return Err(TargetHostRejection::Missing.into());
            }
            let Some(endpoint) = self.round_robin_endpoint(endpoints) else {
                return Err(OagwError::Internal {
                    detail: String::from("the upstream carries no endpoint"),
                }
                .into());
            };
            return Ok(endpoint);
        };
        if !headers::is_valid_target_host(target) {
            return Err(TargetHostRejection::Invalid {
                value: target.to_owned(),
            }
            .into());
        }
        let lowered = target.to_ascii_lowercase();
        endpoints
            .iter()
            .find(|endpoint| endpoint.host() == lowered)
            .ok_or_else(|| {
                Failure::from(TargetHostRejection::Unknown {
                    value: target.to_owned(),
                })
            })
    }

    /// Round-robin selection of an explicit multi-endpoint pool. `None` when
    /// the pool is empty, which the control plane never registers.
    fn round_robin_endpoint<'a>(&self, endpoints: &'a [Endpoint]) -> Option<&'a Endpoint> {
        let first = endpoints.first()?;
        if endpoints.len() <= 1 {
            return Some(first);
        }
        let index =
            self.round_robin.fetch_add(1, Ordering::Relaxed) % endpoints.len().max(1) as u64;
        endpoints.get(index as usize).or(Some(first))
    }

    // ---------------------------------------------------------------- plugins

    /// Runs the request-side plugin phases: guards, auth, request transforms.
    async fn run_request_plugins(
        &self,
        chain: &[PluginRef],
        ctx: &mut RequestContext,
    ) -> Result<(), Failure> {
        for reference in chain {
            match reference.kind() {
                PluginKind::Guard => {
                    let Some(plugin) = self.registry.guard(reference) else {
                        return Err(unresolved_plugin(reference));
                    };
                    if let GuardDecision::Reject(error) =
                        plugin.guard_request(ctx).await.map_err(Failure::Gateway)?
                    {
                        return Err(Failure::Gateway(error));
                    }
                }
                PluginKind::Auth => {
                    let Some(plugin) = self.registry.auth(reference) else {
                        return Err(unresolved_plugin(reference));
                    };
                    plugin.authenticate(ctx).await.map_err(Failure::Gateway)?;
                }
                PluginKind::Transform => {
                    let Some(plugin) = self.registry.transform(reference) else {
                        return Err(unresolved_plugin(reference));
                    };
                    plugin
                        .transform_request(ctx)
                        .await
                        .map_err(Failure::Gateway)?;
                }
            }
        }
        Ok(())
    }

    /// Runs the response-side plugin phases: response guards, transforms.
    async fn run_response_plugins(
        &self,
        chain: &[PluginRef],
        ctx: &mut ResponseContext,
    ) -> Result<(), Failure> {
        for reference in chain {
            match reference.kind() {
                PluginKind::Guard => {
                    let Some(plugin) = self.registry.guard(reference) else {
                        return Err(unresolved_plugin(reference));
                    };
                    if let GuardDecision::Reject(error) =
                        plugin.guard_response(ctx).await.map_err(Failure::Gateway)?
                    {
                        return Err(Failure::Gateway(error));
                    }
                }
                PluginKind::Transform => {
                    let Some(plugin) = self.registry.transform(reference) else {
                        return Err(unresolved_plugin(reference));
                    };
                    plugin
                        .transform_response(ctx)
                        .await
                        .map_err(Failure::Gateway)?;
                }
                PluginKind::Auth => {}
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- responses

    /// Permissive local answer to a CORS preflight
    /// (`docs/ADR/0004-cors.md` "Preflight Request Handling").
    fn preflight_response(&self, call: &ForwardedCall) -> axum::response::Response {
        let origin = origin_of(&call.headers).unwrap_or_else(|| String::from("*"));
        let requested_method = call
            .headers
            .get(http::header::ACCESS_CONTROL_REQUEST_METHOD)
            .and_then(|value| value.to_str().ok());
        let requested_headers = call
            .headers
            .get(http::header::ACCESS_CONTROL_REQUEST_HEADERS)
            .and_then(|value| value.to_str().ok());
        // The preflight is permissive: the requested method is echoed whether
        // or not it is in `allowed_methods` (validation is deferred to the
        // actual request).
        let method = requested_method.unwrap_or_else(|| call.method.as_str());
        let headers = cors::preflight_headers(&origin, method, requested_headers);
        render(StatusCode::NO_CONTENT, headers, Body::empty())
    }

    /// Passes an upstream response through to the caller.
    async fn render_upstream_response(
        &self,
        response: http::Response<hyper::body::Incoming>,
        scope: &Scope,
        policy: &EffectivePolicy,
        response_rules: Option<&ResponseHeaderRules>,
        request_id: Option<String>,
    ) -> axum::response::Response {
        let (parts, incoming) = response.into_parts();
        let mut headers = headers::outbound_response_headers(&parts.headers, response_rules);
        let mut ctx = ResponseContext {
            status: parts.status,
            headers: headers.clone(),
            request_id,
            config: scope.plugin_config(),
        };
        if self
            .run_response_plugins(&scope.plugins, &mut ctx)
            .await
            .is_ok()
        {
            headers = ctx.headers;
        }
        if let Some(cors_config) = enabled_cors(policy) {
            let origin = origin_of(&scope.call.headers).unwrap_or_else(|| String::from("*"));
            cors::write_response_headers(cors_config, &origin, &mut headers);
        }
        for (name, value) in scope.rate_headers.iter() {
            headers.insert(name, value.clone());
        }
        if let Some(request_id) = ctx.request_id.as_ref()
            && !headers.contains_key(REQUEST_ID_HEADER)
            && let Ok(value) = HeaderValue::from_str(request_id)
        {
            headers.insert(REQUEST_ID_HEADER, value);
        }
        headers.insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
        let idle = streaming::idle_timeout(&parts.headers, self.config.proxy_timeout());
        let body = streaming::response_body(incoming, idle);
        render(parts.status, headers, body)
    }

    /// Forwards an upgrade request and splices the connection it opens.
    ///
    /// The proxy timeout covers the handshake only: once the upstream has
    /// answered `101 Switching Protocols`, the connection is spliced byte for
    /// byte between the caller and the upstream without any timeout
    /// (`docs/PRD.md` §5.4, `cpt-cf-oagw-fr-streaming`).
    async fn forward_upgrade(
        &self,
        scope: &mut Scope,
        request: http::Request<Body>,
    ) -> Result<axum::response::Response, Failure> {
        // An upgrade the connection cannot carry is refused before the
        // upstream is dialed: the handshake could never be completed.
        let Some(on_client) = scope.call.upgrade.take() else {
            return Err(OagwError::Validation {
                detail: String::from(
                    "the inbound connection does not support the requested protocol upgrade",
                ),
            }
            .into());
        };
        let mut response = send(&self.client, request, self.config.proxy_timeout()).await?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(OagwError::ProtocolError {
                detail: format!(
                    "the upstream refused the protocol upgrade: {}",
                    response.status()
                ),
            }
            .into());
        }
        let mut headers =
            streaming::switching_protocols_headers(response.headers(), &scope.call.headers);
        // The `101` head is the whole answer a plugin ever sees: the response
        // stage runs on it, exactly as it does on a buffered answer, and the
        // spliced bytes are never handed to a plugin
        // (`docs/ADR/0002-plugin-system.md` "Execution Order").
        let mut ctx = ResponseContext {
            status: StatusCode::SWITCHING_PROTOCOLS,
            headers: headers.clone(),
            request_id: scope.request_id.clone(),
            config: scope.plugin_config(),
        };
        if self
            .run_response_plugins(&scope.plugins, &mut ctx)
            .await
            .is_ok()
        {
            headers = ctx.headers;
        }
        for (name, value) in scope.rate_headers.iter() {
            headers.insert(name, value.clone());
        }
        if let Some(request_id) = scope.request_id.as_ref()
            && let Ok(value) = HeaderValue::from_str(request_id)
        {
            headers.entry(REQUEST_ID_HEADER).or_insert(value);
        }
        headers.insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
        let on_upstream = streaming::upstream_upgrade(&mut response);
        // The body of a `101` is empty: dropping the answer frees the
        // connection for the splice that follows.
        drop(response);
        tokio::spawn(streaming::splice(
            on_client,
            on_upstream,
            Arc::clone(&self.cancelled),
        ));
        Ok(render(
            StatusCode::SWITCHING_PROTOCOLS,
            headers,
            Body::empty(),
        ))
    }

    /// Renders a gateway failure as an RFC 9457 problem response.
    async fn render_failure(&self, failure: Failure, scope: &Scope) -> axum::response::Response {
        let mut headers = scope.rate_headers.clone();
        if let Some(request_id) = &scope.request_id
            && let Ok(value) = HeaderValue::from_str(request_id)
        {
            headers.entry(REQUEST_ID_HEADER).or_insert(value);
        }
        for reference in &scope.plugins {
            if reference.kind() != PluginKind::Transform {
                continue;
            }
            let Some(plugin) = self.registry.transform(reference) else {
                continue;
            };
            let mut ctx = ErrorContext {
                error: error_of(&failure),
                headers: headers.clone(),
                request_id: scope.request_id.clone(),
                config: scope.plugin_config(),
            };
            if plugin.transform_error(&mut ctx).await.is_ok() {
                headers = ctx.headers;
            }
        }
        headers.insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        match failure {
            Failure::Gateway(error) => {
                if let Some(seconds) = error.retry_after_secs()
                    && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
                {
                    headers.insert(http::header::RETRY_AFTER, value);
                }
                render_problem(&error.problem(scope.extensions()), headers)
            }
            Failure::Documented(documented) => {
                render_documented(&documented, scope.extensions(), headers)
            }
        }
    }
}

// ------------------------------------------------------------------ upstream

/// Type-erased upstream client of the data plane.
type UpstreamClient = Client<hyper_util::client::legacy::connect::HttpConnector, Body>;

/// Sends the request to the upstream, enforcing the proxy timeout.
///
/// The timeout covers connection establishment, the request write and the
/// response head; the body of a streaming response is forwarded without one.
async fn send(
    client: &UpstreamClient,
    request: http::Request<Body>,
    timeout: std::time::Duration,
) -> Result<http::Response<hyper::body::Incoming>, OagwError> {
    tokio::time::timeout(timeout, client.request(request))
        .await
        .map_err(|_| OagwError::RequestTimeout {
            detail: format!("the upstream did not answer within {timeout:?}"),
        })?
        .map_err(|error| {
            if error.is_connect() {
                OagwError::ProtocolError {
                    detail: format!("the upstream connection failed: {error}"),
                }
            } else {
                OagwError::ProtocolError {
                    detail: format!("the upstream request failed: {error}"),
                }
            }
        })
}

/// Builds the absolute URI of the upstream call.
fn build_uri(endpoint: &Endpoint, path: &str, query: Option<&str>) -> Result<Uri, OagwError> {
    let base = endpoint.base_url();
    let url = match query {
        Some(query) => format!("{base}{path}?{query}"),
        None => format!("{base}{path}"),
    };
    Uri::try_from(url.as_str()).map_err(|error| OagwError::Validation {
        detail: format!("the forwarded request URI is invalid: {error}"),
    })
}

/// Buffered request body of a proxied call, enforcing the body limit.
///
/// The limit is enforced *before* buffering
/// (`cpt-cf-oagw-constraint-body-limit`): a declared length above the limit is
/// rejected without reading a byte, and an undeclared one as soon as the limit
/// is crossed.
///
/// # Errors
///
/// Returns [`OagwError::PayloadTooLarge`] when the body exceeds `limit`, and
/// [`OagwError::Validation`] when the body cannot be read.
pub(crate) async fn read_body(body: &mut Body, limit: u64) -> Result<Bytes, OagwError> {
    if let Some(declared) = axum::body::HttpBody::size_hint(body).exact()
        && declared > limit
    {
        return Err(OagwError::PayloadTooLarge { limit_bytes: limit });
    }
    let capped = usize::try_from(limit.saturating_add(1)).unwrap_or(usize::MAX);
    let bytes = axum::body::to_bytes(std::mem::take(body), capped)
        .await
        .map_err(|error| OagwError::Validation {
            detail: format!("the request body could not be read: {error}"),
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(OagwError::PayloadTooLarge { limit_bytes: limit });
    }
    Ok(bytes)
}

/// `X-Request-ID`-free origin of a request, trimmed.
fn origin_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The effective CORS configuration when CORS handling is on.
fn enabled_cors(policy: &EffectivePolicy) -> Option<&crate::domain::CorsConfig> {
    policy.cors.as_ref().filter(|config| config.enabled)
}

/// `true` when a limit rejects the request; the `queue` and `degrade`
/// strategies are not implemented yet and fall back to `reject`.
fn rejects(limit: &crate::domain::RateLimitConfig) -> bool {
    !matches!(limit.strategy, RateLimitStrategy::Degrade)
}

/// Lowercases an alias and strips a trailing dot
/// (`docs/DESIGN.md` §3.2 "Alias Normalization").
fn normalize_alias(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// `503 PluginNotFound` for a chain entry with no native implementation.
fn unresolved_plugin(reference: &PluginRef) -> Failure {
    Failure::Gateway(OagwError::PluginNotFound {
        plugin_ref: reference.as_ref_str().into_owned(),
    })
}

/// The error handed to the error transforms of a failure.
fn error_of(failure: &Failure) -> OagwError {
    match failure {
        Failure::Gateway(error) => error.clone(),
        Failure::Documented(documented) => OagwError::Validation {
            detail: documented.detail.clone(),
        },
    }
}

/// Path forwarded to the upstream: the matched route prefix plus the request
/// suffix (`docs/DESIGN.md` §3.1 "Path suffix … Append to `match.http.path`").
fn forwarded_path(route: &Route, suffix: &str) -> String {
    let Some(matched) = route.http_match() else {
        return String::from("/");
    };
    let Some((_prefix, appended)) = matched.split_path(suffix) else {
        return matched.path.clone();
    };
    if appended.is_empty() || appended == "/" {
        matched.path.clone()
    } else {
        format!("{}{}", matched.path, appended)
    }
}

/// The plugin configuration slot of a proxied request.
fn plugin_config(policy: &EffectivePolicy) -> Value {
    policy
        .auth
        .as_ref()
        .map(|auth| auth.config.clone())
        .unwrap_or(Value::Null)
}

// ----------------------------------------------------------------- rendering

/// Content type of an RFC 9457 problem body.
const APPLICATION_PROBLEM_JSON: &str = "application/problem+json";

/// Renders a problem body.
fn render_problem(problem: &Problem, mut headers: HeaderMap) -> axum::response::Response {
    let body = serde_json::to_value(problem)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| String::from("{}"));
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static(APPLICATION_PROBLEM_JSON),
    );
    render(
        StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        headers,
        Body::from(body),
    )
}

/// Renders a documented error of its own GTS type.
fn render_documented(
    error: &DocumentedError,
    extensions: ProblemExtensions,
    headers: HeaderMap,
) -> axum::response::Response {
    let mut context = serde_json::Map::new();
    context.insert(
        String::from("error_source"),
        serde_json::Value::String(String::from(ERROR_SOURCE_GATEWAY)),
    );
    if let Some(upstream_id) = extensions.upstream_id {
        context.insert(
            String::from("upstream_id"),
            serde_json::Value::String(upstream_id.to_string()),
        );
    }
    if let Some(host) = &extensions.host {
        context.insert(
            String::from("host"),
            serde_json::Value::String(host.clone()),
        );
    }
    if let Some(path) = &extensions.path {
        context.insert(
            String::from("path"),
            serde_json::Value::String(path.clone()),
        );
    }
    let problem = Problem {
        problem_type: String::from(error.gts_type_id),
        title: String::from(error.title),
        status: error.status,
        detail: error.detail.clone(),
        instance: extensions.instance,
        trace_id: None,
        context: serde_json::Value::Object(context),
        error_code: Some(String::from(error.code)),
        error_domain: Some(String::from(ERROR_DOMAIN)),
    };
    render_problem(&problem, headers)
}

/// Builds the axum response of a rendered body.
fn render(status: StatusCode, headers: HeaderMap, body: Body) -> axum::response::Response {
    let mut response = axum::response::Response::builder().status(status);
    for (name, value) in headers.iter() {
        response = response.header(name, value);
    }
    response.body(body).unwrap_or_else(|error| {
        tracing::error!(%error, "the response could not be rendered");
        axum::response::Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::empty())
            .unwrap_or_default()
    })
}

// --------------------------------------------------------------------- routes

/// Splits a proxy path into its alias and forwarded suffix.
///
/// `/oagw/v1/proxy/api.openai.com/v1/chat` becomes
/// `("api.openai.com", "/v1/chat")`; a bare alias forwards `/`.
#[must_use]
pub fn split_proxy_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix(PROXY_ALIAS_PREFIX)?;
    match rest.split_once('/') {
        Some((alias, suffix)) => Some((alias.to_owned(), format!("/{suffix}"))),
        None => Some((rest.to_owned(), String::from("/"))),
    }
}

/// Registers the proxy surface on the gear router.
///
/// The route is *gear-relative* and served verbatim: `ANY
/// /oagw/v1/proxy/{alias}` with an optional trailing path and query. It is
/// authenticated (the host middleware supplies the
/// [`SecurityContext`]) but not a management operation, so it publishes no
/// OpenAPI operation.
pub fn register_proxy_routes(router: axum::Router, plane: Arc<DataPlane>) -> axum::Router {
    let proxy = axum::routing::any(proxy_handler).layer(axum::Extension(plane));
    router.route(PROXY_ROUTE_PATH, proxy)
}

/// Handler of the proxy surface: projects the request onto a
/// [`ProxyCall`] and hands it to the data plane.
/// Client address of a proxied request, `None` when the host does not serve
/// the router with `into_make_service_with_connect_info`.
///
/// `ConnectInfo<T>` itself cannot be optional in axum `0.8`, so the extension
/// is read directly: a missing one never rejects the request, and the `ip`
/// rate-limit scope falls back to the tenant
/// (`docs/ADR/0003-rate-limiting.md` "Scope").
struct PeerAddress(Option<IpAddr>);

impl<S> FromRequestParts<S> for PeerAddress
where
    S: Send + Sync,
{
    /// Never fails: an absent peer address is a `None` field.
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|peer| peer.0.ip()),
        ))
    }
}

async fn proxy_handler(
    Extension(plane): Extension<Arc<DataPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    PeerAddress(peer): PeerAddress,
    request: axum::extract::Request,
) -> axum::response::Response {
    // The request is taken as a whole so that the upgrade handle the server
    // put into its extensions is captured before the pipeline runs: an
    // upgrade can only be answered from the same connection.
    let (mut parts, body) = request.into_parts();
    let upgrade = InboundUpgrade::capture(&mut parts);
    let (method, uri, headers) = (parts.method, parts.uri, parts.headers);
    let Some((alias, suffix)) = split_proxy_path(uri.path()) else {
        // Unreachable through the mounted route: `{*alias}` requires a
        // non-empty alias segment.
        let mut headers = HeaderMap::new();
        headers.insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        return render_problem(
            &OagwError::Validation {
                detail: String::from("the proxy path carries no alias"),
            }
            .problem(ProblemExtensions::default()),
            headers,
        );
    };
    let call = ProxyCall {
        alias,
        suffix,
        method,
        path: uri.path().to_owned(),
        query: uri.query().map(str::to_owned),
        headers,
        body,
        tenant_id: ctx.subject_tenant_id(),
        subject_id: ctx.subject_id(),
        // The peer address exists only when the host serves the router with
        // `into_make_service_with_connect_info`; otherwise the `ip` rate-limit
        // scope falls back to the tenant.
        client_ip: peer.map(|peer| peer.to_string()),
        upgrade,
    };
    plane.execute(call).await
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "proxy_tests.rs"]
mod proxy_tests;
