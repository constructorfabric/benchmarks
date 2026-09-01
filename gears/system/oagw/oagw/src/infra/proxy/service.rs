// Created: 2026-08-29 by Constructor Tech
//! Data-plane proxy engine.
//!
//! The pipeline order is normative (contract §5): alias resolution → route
//! match → CORS preflight → hierarchical merge → auth → rate limit → guard →
//! transform → circuit breaker → forward → response plugins → response header
//! rules.
//!
//! Security: no request or response body, query string or header value is ever
//! logged; credentials are injected into the outbound request only and are
//! never formatted into an error message.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures_util::Stream;
use hyper::body::{Body as HttpBody, Frame};
use toolkit_http::{HttpClient, HttpClientBuilder, HttpClientConfig, HttpResponse};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::merge::EffectiveConfig;
use crate::domain::model::{
    CorsConfig, Endpoint, HeadersConfig, MAX_BODY_BYTES, PathSuffixMode, RateLimitConfig, Route,
    Upstream,
};
use crate::domain::plugin::{PluginBinding, PluginError, RequestContext, ResponseContext};
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::PluginRegistry;
use crate::infra::proxy::circuit_breaker::{BreakerCheck, CircuitBreakerRegistry, breaker_open};
use crate::infra::proxy::headers::{
    ERROR_SOURCE_HEADER, HOP_BY_HOP, TARGET_HOST_HEADER, apply_response_rules, bare_host,
    build_request_headers, strip_hop_by_hop,
};
use crate::infra::proxy::rate_limiter::{RateDecision, RateLimiterRegistry, keyed_scope, rate_key};

type DataPlaneResult<T> = Result<T, ProxyFailure>;

/// `text/event-stream` media type.
const SSE_MEDIA_TYPE: &str = "text/event-stream";

/// Detail of a `404 RouteNotFound` (no interpolated request values).
const NO_ROUTE: &str = "no enabled route matches this request";

/// `authorization`, forwarded only when the header rules allow it.
const AUTHORIZATION: &str = "authorization";

/// A proxied request in transport-neutral form.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// Request method.
    pub method: String,
    /// Routing alias taken from the URL.
    pub alias: String,
    /// Raw remainder of the path after the alias (no leading slash).
    pub path_suffix: String,
    /// Raw query string, when present.
    pub query: Option<String>,
    /// Inbound request headers.
    pub headers: axum::http::HeaderMap,
    /// Buffered request body.
    pub body: Bytes,
    /// Caller tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject.
    pub subject_id: Uuid,
    /// Client ip (empty when the host does not expose peer addresses).
    pub client_ip: String,
    /// Request URI used as the RFC 9457 `instance`.
    pub instance: String,
    /// Correlation id.
    pub trace_id: String,
    /// Caller security context (credential resolution only).
    pub security: SecurityContext,
    /// Normalized path pattern of the matched route, set during resolution.
    ///
    /// Observability only: `http.route` carries the pattern rather than the
    /// raw request path so metric label cardinality stays bounded (DESIGN
    /// §4.2). `None` until a route matches.
    pub route_pattern: Option<String>,
}

impl ProxyRequest {
    /// `X-OAGW-Target-Host` when the caller selected a specific endpoint.
    ///
    /// The value is reported as the caller sent it: `select_endpoint` validates
    /// it against ADR-0007 (a bare hostname or IP, no port, path or scheme) and
    /// has to see a port-bearing value to report `invalid_target_host.v1`
    /// instead of silently routing to the host the port was stripped from.
    #[must_use]
    pub fn target_host(&self) -> Option<String> {
        self.headers
            .get(TARGET_HOST_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(ToOwned::to_owned)
    }
}

/// A gateway failure with optional upstream routing context.
#[derive(Debug, Clone)]
pub struct ProxyFailure {
    /// Domain error.
    pub kind: OagwError,
    /// Upstream the request would have been routed to.
    pub upstream_id: Option<Uuid>,
    /// Upstream host the request targeted.
    pub host: Option<String>,
    /// Rate-limit budget the rejected request would have spent (ADR-0003).
    pub rate: Option<Box<RateReport>>,
}

/// The `X-RateLimit-*` counters reported on a `429` (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateReport {
    /// Configured limit.
    pub limit: u64,
    /// Tokens left in the bucket.
    pub remaining: u64,
    /// Unix epoch when the bucket is refilled.
    pub reset_epoch: u64,
}

impl ProxyFailure {
    /// Wrap a domain error with no routing context.
    #[must_use]
    pub fn new(kind: OagwError) -> Self {
        Self {
            kind,
            upstream_id: None,
            host: None,
            rate: None,
        }
    }

    /// Attach the upstream routing context.
    #[must_use]
    pub fn with_upstream(mut self, upstream_id: Uuid) -> Self {
        self.upstream_id = Some(upstream_id);
        self
    }

    /// Attach the rate-limit counters a rejected request reports.
    #[must_use]
    pub fn with_rate(mut self, decision: &RateDecision) -> Self {
        self.rate = Some(Box::new(RateReport {
            limit: decision.limit,
            remaining: decision.remaining,
            reset_epoch: decision.reset_epoch,
        }));
        self
    }

    /// Attach the upstream host.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }
}

impl From<OagwError> for ProxyFailure {
    fn from(kind: OagwError) -> Self {
        Self::new(kind)
    }
}

/// Error-carrying byte stream handed to the transport layer.
type PinnedErrorStream = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, OagwError>> + Send>>;

/// Body of a proxied response.
pub enum ProxyBody {
    /// Fully buffered body.
    Full(Bytes),
    /// Streaming body; stream errors carry the abort reason.
    Stream(PinnedErrorStream),
}

impl std::fmt::Debug for ProxyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(bytes) => f.debug_tuple("Full").field(&bytes.len()).finish(),
            Self::Stream(_) => f.write_str("Stream(..)"),
        }
    }
}

/// A proxied response ready for the wire.
#[derive(Debug)]
pub struct ProxyOutcome {
    /// Upstream status code.
    pub status: axum::http::StatusCode,
    /// Response headers after transformation.
    pub headers: axum::http::HeaderMap,
    /// Response body.
    pub body: ProxyBody,
    /// Upstream the request was routed to.
    pub upstream_id: Uuid,
    /// Upstream host that served the request.
    pub host: String,
}

/// Target resolved for the WebSocket leg.
#[derive(Debug)]
pub struct WebSocketLeg {
    /// `wss://` / `ws://` upstream URL.
    pub url: String,
    /// Headers to send on the upstream handshake.
    pub headers: Vec<(String, String)>,
    /// Upstream id.
    pub upstream_id: Uuid,
    /// Upstream host.
    pub host: String,
}

/// Everything the data plane needs, wired at gear init.
pub struct DataPlaneService {
    control_plane: Arc<ControlPlaneService>,
    plugins: Arc<PluginRegistry>,
    tenant_resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
    rate_limiters: RateLimiterRegistry,
    breakers: CircuitBreakerRegistry,
    http: HttpClient,
    config: OagwConfig,
    round_robin: AtomicU64,
    metrics: Arc<dyn crate::domain::ports::OagwMetricsPort>,
}

impl std::fmt::Debug for DataPlaneService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlaneService")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// A fully resolved proxy target.
struct Resolved {
    upstream: Upstream,
    effective: EffectiveConfig,
    route: Route,
    remainder: String,
}

impl DataPlaneService {
    /// Build the service.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Internal`] when the outbound HTTP client cannot be
    /// initialised (TLS backend failure).
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        plugins: Arc<PluginRegistry>,
        tenant_resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
        config: OagwConfig,
    ) -> Result<Self, OagwError> {
        Self::with_metrics(
            control_plane,
            plugins,
            tenant_resolver,
            config,
            Arc::new(crate::infra::metrics::OagwMetricsMeter::from_global()),
        )
    }

    /// Build the service with an explicit observability adapter.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Internal`] when the outbound HTTP client cannot be
    /// initialised (TLS backend failure).
    pub fn with_metrics(
        control_plane: Arc<ControlPlaneService>,
        plugins: Arc<PluginRegistry>,
        tenant_resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
        config: OagwConfig,
        metrics: Arc<dyn crate::domain::ports::OagwMetricsPort>,
    ) -> Result<Self, OagwError> {
        let mut http_config = HttpClientConfig::proxy();
        http_config.request_timeout = config.proxy_timeout();
        let http = HttpClientBuilder::with_config(http_config)
            .build()
            .map_err(|error| OagwError::Internal(format!("outbound http client: {error}")))?;
        let breakers = CircuitBreakerRegistry::default();
        Ok(Self {
            control_plane,
            plugins,
            tenant_resolver,
            rate_limiters: RateLimiterRegistry::new(),
            breakers,
            http,
            config,
            round_robin: AtomicU64::new(0),
            metrics,
        })
    }

    /// The plugin registry (tests install additional plugins through it).
    #[must_use]
    pub fn plugins(&self) -> &PluginRegistry {
        &self.plugins
    }

    /// The configured knobs.
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// The circuit-breaker registry.
    #[must_use]
    pub fn breakers(&self) -> &CircuitBreakerRegistry {
        &self.breakers
    }

    /// The rate-limit registry.
    #[must_use]
    pub fn rate_limiters(&self) -> &RateLimiterRegistry {
        &self.rate_limiters
    }

    // ---------------------------------------------------------------------
    // tenant chain
    // ---------------------------------------------------------------------

    async fn tenant_chain(&self, security: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        let Some(resolver) = self.tenant_resolver.as_ref() else {
            return vec![tenant_id];
        };
        let request = tenant_resolver_sdk::TenantId(tenant_id);
        let response = resolver
            .get_ancestors(
                security,
                request,
                &tenant_resolver_sdk::GetAncestorsOptions::default(),
            )
            .await;
        let Ok(response) = response else {
            // A resolver failure must not turn every request into a 404:
            // degrade to the caller's own tenant.
            return vec![tenant_id];
        };
        let mut chain: Vec<Uuid> = Vec::with_capacity(response.ancestors.len() + 1);
        chain.push(response.tenant.id.0);
        for ancestor in &response.ancestors {
            chain.push(ancestor.id.0);
        }
        chain
    }

    // ---------------------------------------------------------------------
    // entry point
    // ---------------------------------------------------------------------

    /// Run the proxy pipeline.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyFailure`] for every gateway-side error in the contract's
    /// error table.
    pub async fn handle(&self, mut request: ProxyRequest) -> DataPlaneResult<ProxyOutcome> {
        let started = std::time::Instant::now();
        let outcome = self.dispatch(&mut request).await;
        self.audit(&request, &outcome, started);
        self.metrics_request(&request, &outcome, started);
        outcome
    }

    /// Record the DESIGN §4.2 request, error and rate-limit instruments.
    fn metrics_request(
        &self,
        request: &ProxyRequest,
        outcome: &DataPlaneResult<ProxyOutcome>,
        started: std::time::Instant,
    ) {
        let duration = started.elapsed().as_secs_f64();
        // `host` is the upstream alias and `http.route` the normalized route
        // pattern (never the raw path), so label cardinality stays bounded.
        let host = outcome
            .as_ref()
            .ok()
            .map_or_else(|| request.alias.as_str(), |built| built.host.as_str());
        let route = request.route_pattern.as_deref().unwrap_or("unmatched");
        match outcome {
            Ok(built) => {
                self.metrics.record_request(
                    host,
                    route,
                    &request.method,
                    built.status.as_u16(),
                    duration,
                );
            }
            Err(failure) => {
                let error_type = crate::domain::error::error_type(failure.kind.type_suffix());
                self.metrics.record_request(
                    host,
                    route,
                    &request.method,
                    failure.kind.status(),
                    duration,
                );
                self.metrics.record_error(host, route, &error_type);
                if failure.rate.is_some() {
                    self.metrics.record_rate_limit_exceeded(host, route);
                }
            }
        }
    }

    /// The pipeline itself, so [`Self::handle`] can audit every outcome.
    async fn dispatch(&self, request: &mut ProxyRequest) -> DataPlaneResult<ProxyOutcome> {
        // A browser preflight carries no credentials (WHATWG Fetch), so there is
        // no tenant context to resolve an alias or a route with (ADR-0004). It is
        // answered permissively before any resolution and validation is deferred
        // to the actual request that follows it.
        if preflight_requested(request) {
            return Ok(preflight_response(request));
        }
        let resolved = self.resolve(request).await?;
        let cors = resolved.effective.cors.as_ref();
        if let Some(error) = cors_error(cors, request) {
            return Err(ProxyFailure::new(error).with_upstream(resolved.upstream.id));
        }
        self.execute(request, resolved).await
    }

    /// Emit the DESIGN §4.3 audit event of one proxy request.
    fn audit(
        &self,
        request: &ProxyRequest,
        outcome: &DataPlaneResult<ProxyOutcome>,
        started: std::time::Instant,
    ) {
        let (status, response_size, error_type) = match outcome {
            Ok(built) => (
                Some(built.status),
                match &built.body {
                    ProxyBody::Full(bytes) => bytes.len(),
                    // A relayed stream has no known length; its frames are
                    // never counted or read here.
                    ProxyBody::Stream(_) => 0,
                },
                None,
            ),
            Err(failure) => (
                Some(
                    axum::http::StatusCode::from_u16(failure.kind.status())
                        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
                ),
                0,
                Some(crate::domain::error::error_type(failure.kind.type_suffix())),
            ),
        };
        crate::infra::audit::proxy_request(&crate::infra::audit::ProxyAudit {
            request_id: &request.trace_id,
            tenant_id: request.tenant_id,
            principal_id: request.subject_id,
            alias: &request.alias,
            method: &request.method,
            status,
            duration_ms: started.elapsed().as_millis(),
            request_size: request.body.len(),
            response_size,
            error_type: error_type.as_deref(),
        });
    }

    /// Resolve the alias, the tenant chain and the matching route.
    async fn resolve(&self, request: &mut ProxyRequest) -> DataPlaneResult<Resolved> {
        let chain = self
            .tenant_chain(&request.security, request.tenant_id)
            .await;
        let candidates = self
            .control_plane
            .upstream_candidates(&chain, &request.alias);
        let Some((closest, closest_config)) = candidates.first() else {
            return Err(ProxyFailure::new(OagwError::RouteNotFound(format!(
                "no upstream is registered for alias '{}'",
                request.alias
            ))));
        };
        if !closest_config.enabled {
            return Err(ProxyFailure::new(OagwError::LinkUnavailable(
                "upstream is disabled".to_owned(),
            ))
            .with_upstream(closest.id));
        }
        // The closest tenant owns the routing target (DESIGN §3.3 "shadowing
        // selects the routing target only"), while the matching route is
        // inherited down the chain: walk descendant → root and take the first
        // tenant whose routes match. A tenant that registered only the
        // upstream therefore runs on the ancestor's route definition.
        let request_path = normalized_request_path(&request.path_suffix);
        for (upstream, _base) in &candidates {
            let routes = self.control_plane.routes_for_upstream(upstream.id);
            let matched = if preflight_requested(request) {
                // A preflight carries `OPTIONS`, which the route allowlist
                // rarely names (ADR-0004): fall back to a path-only match so
                // the route's CORS configuration is still what answers the
                // browser.
                match_route(&routes, &request.method, &request_path)
                    .or_else(|_| match_preflight_route(&routes, &request_path))
            } else {
                match_route(&routes, &request.method, &request_path)
            };
            if let Ok((route, remainder)) = matched {
                request.route_pattern = Some(
                    route
                        .spec
                        .match_config
                        .http
                        .as_ref()
                        .map_or_else(|| "unmatched".to_owned(), |http| http.path.clone()),
                );
                return Ok(Resolved {
                    upstream: closest.clone(),
                    effective: with_route(closest_config, &route),
                    route,
                    remainder,
                });
            }
        }
        Err(ProxyFailure::new(OagwError::RouteNotFound(
            NO_ROUTE.to_owned(),
        )))
    }

    /// Run the request leg after alias / route / CORS resolution.
    async fn execute(
        &self,
        request: &ProxyRequest,
        resolved: Resolved,
    ) -> DataPlaneResult<ProxyOutcome> {
        let Resolved {
            upstream,
            effective,
            route,
            remainder,
        } = resolved;
        let mut context = self.request_context(request, &upstream);
        // The allowlist governs what the caller sent; plugins may append to the
        // filtered query afterwards (e.g. an apikey delivered as a query
        // parameter) and are not subject to it.
        context.query = filtered_query(&route, request.query.as_deref());
        if let Some(binding) = effective.auth.as_ref().map(auth_binding) {
            self.authenticate(&mut context, &binding, upstream.id)
                .await?;
        }
        let rate_decision = self.enforce_rate_limit(
            effective.rate_limit.as_ref(),
            effective.rate_limit_owner.unwrap_or(upstream.id),
            request,
            upstream.id,
        )?;
        self.run_request_plugins(&mut context, &effective.plugins, upstream.id, true)
            .await?;

        let endpoint = self.select_endpoint(&upstream, request.target_host())?;
        self.check_target(&endpoint, upstream.id)?;
        let path = target_path(&route, &remainder)?;
        let query = context.query.clone();
        let host = endpoint.host.clone();
        self.check_breaker(upstream.id, &host)?;

        let url = target_url(&endpoint, &path, query.as_deref());
        let outcome = self
            .send(
                request.method.as_str(),
                &url,
                outbound_headers(
                    &context.headers,
                    &effective.headers.request,
                    &request.headers,
                ),
                request.body.clone(),
            )
            .await;

        match outcome {
            Ok(response) => {
                self.breaker_success(upstream.id, &host);
                // What makes the leg a stream is the body the upstream is
                // actually producing (`text/event-stream`), not what the caller
                // asked for: an `accept: text/event-stream` on a JSON reply must
                // still go through the buffered response phases (ADR-0002).
                let mut built = if is_streaming(response.headers()) {
                    self.streaming_response(&upstream, response).await
                } else {
                    self.buffered_response(
                        &upstream,
                        &effective.plugins,
                        &effective.headers,
                        response,
                        &mut context,
                    )
                    .await
                };
                if let Ok(outcome) = built.as_mut() {
                    outcome.upstream_id = upstream.id;
                    outcome.host.clone_from(&host);
                    if let Some(decision) = &rate_decision {
                        set_rate_limit_headers(&mut outcome.headers, decision);
                    }
                    if let Some(cors) = effective.cors.as_ref()
                        && let Some(origin) = request_header(request, "origin")
                    {
                        set_cors_response_headers(&mut outcome.headers, cors, origin);
                    }
                }
                built
            }
            Err(error) => {
                self.breaker_failure(upstream.id, &host);
                Err(error.with_upstream(upstream.id).with_host(host))
            }
        }
    }

    /// Assemble the mutable request context handed to the plugin chain.
    fn request_context(&self, request: &ProxyRequest, upstream: &Upstream) -> RequestContext {
        RequestContext {
            tenant_id: upstream.tenant_id,
            upstream_id: upstream.id,
            alias: upstream.alias.clone(),
            method: request.method.clone(),
            path: normalized_request_path(&request.path_suffix),
            query: request.query.clone(),
            // The chain sees the request as the caller sent it (ADR-0002: guards
            // run before transforms, and a presence guard has to be able to see
            // a caller-supplied header). The outbound header rules are applied
            // when the request is composed for the upstream, in `outbound_headers`.
            headers: request.headers.clone(),
            body: request.body.clone(),
            uri: request.instance.clone().parse().unwrap_or_default(),
            config: serde_json::Map::new(),
            security: request.security.clone(),
        }
    }

    /// Run the single auth plugin.
    async fn authenticate(
        &self,
        context: &mut RequestContext,
        binding: &PluginBinding,
        upstream_id: Uuid,
    ) -> DataPlaneResult<()> {
        let plugin = self.plugins.auth_plugin(binding).ok_or_else(|| {
            ProxyFailure::new(OagwError::PluginNotFound(format!(
                "auth plugin '{}' has no implementation",
                binding.plugin_ref
            )))
            .with_upstream(upstream_id)
        })?;
        context.config = binding.config.as_object().cloned().unwrap_or_default();
        let failure = plugin.authenticate(context).await;
        failure.map_err(|error| {
            // DESIGN §4.3: failed authentication attempts are audited. Only the
            // error's type code is recorded, never the presented credential or
            // the message that could embed one.
            crate::infra::audit::auth_failure(
                context.tenant_id,
                upstream_id,
                crate::domain::error::error_type(match &error {
                    PluginError::Authentication(_) => "authentication.failed.v1",
                    other => crate::domain::error::OagwError::from(other.clone()).type_suffix(),
                })
                .as_str(),
            );
            ProxyFailure::new(OagwError::from(error)).with_upstream(upstream_id)
        })
    }

    /// Reject requests over the effective rate limit.
    fn enforce_rate_limit(
        &self,
        config: Option<&RateLimitConfig>,
        owner: Uuid,
        request: &ProxyRequest,
        upstream_id: Uuid,
    ) -> DataPlaneResult<Option<RateDecision>> {
        let Some(config) = config else {
            return Ok(None);
        };
        let scope_key = match config.scope {
            crate::domain::model::RateScope::User => keyed_scope(
                &request.subject_id.to_string(),
                &request.tenant_id.to_string(),
            ),
            crate::domain::model::RateScope::Ip => {
                keyed_scope(&request.client_ip, &request.tenant_id.to_string())
            }
            crate::domain::model::RateScope::Route => owner.to_string(),
            _ => request.tenant_id.to_string(),
        };
        let key = rate_key(config, &scope_key, &owner.to_string());
        let Some(decision) = self.rate_limiters.check(&key, config) else {
            return Ok(None);
        };
        if decision.allowed {
            // Reporting the counters is opt-out, so the caller gets
            // `X-RateLimit-*` on every accepted request by default.
            return Ok(config.response_headers().then_some(decision));
        }
        // ADR-0003 reports the exhausted budget on the 429 itself: the counters
        // travel with the problem document, next to `Retry-After`.
        Err(
            ProxyFailure::new(OagwError::RateLimitExceeded(decision.retry_after.max(1)))
                .with_upstream(upstream_id)
                .with_rate(&decision),
        )
    }

    /// Run guard and transform plugins in chain order.
    ///
    /// `request_phase` selects the guard phase; transforms always run their
    /// request hook.
    async fn run_request_plugins(
        &self,
        context: &mut RequestContext,
        config: &crate::domain::model::PluginsConfig,
        upstream_id: Uuid,
        request_phase: bool,
    ) -> DataPlaneResult<()> {
        for binding in plugin_bindings(config) {
            if self.plugins.missing(&binding.plugin_ref) {
                // A chain entry that resolves to no implementation fails closed
                // rather than being silently skipped (contract §9).
                return Err(ProxyFailure::new(OagwError::PluginNotFound(format!(
                    "plugin '{}' has no implementation",
                    binding.plugin_ref
                )))
                .with_upstream(upstream_id));
            }
            context.config = binding.config.as_object().cloned().unwrap_or_default();
            if request_phase && let Some(guard) = self.plugins.guard_plugin(&binding) {
                let decision = guard.guard_request(context).await;
                reject_or_allow(decision, upstream_id)?;
            }
            if let Some(transform) = self.plugins.transform_plugin(&binding) {
                transform
                    .transform_request(context)
                    .await
                    .map_err(|error| {
                        ProxyFailure::new(OagwError::from(error)).with_upstream(upstream_id)
                    })?;
            }
        }
        Ok(())
    }

    /// Resolve the endpoint serving this request.
    fn select_endpoint(
        &self,
        upstream: &Upstream,
        requested: Option<String>,
    ) -> DataPlaneResult<Endpoint> {
        let endpoints = &upstream.spec.server.endpoints;
        let selected = match endpoints.as_slice() {
            [] => Err(ProxyFailure::new(OagwError::Validation(
                "upstream has no endpoints".to_owned(),
            ))),
            [single] => match requested.as_deref() {
                Some(host) => match normalized_target_host(host) {
                    Some(host) if single.normalized_host() == host => Ok(single.clone()),
                    Some(host) => Err(ProxyFailure::new(unknown_target_host(&host, endpoints))),
                    None => Err(ProxyFailure::new(invalid_target_host(host))),
                },
                _ => Ok(single.clone()),
            },
            many => match requested.as_deref() {
                Some(host) => match normalized_target_host(host) {
                    Some(host) => many
                        .iter()
                        .find(|endpoint| endpoint.normalized_host() == host)
                        .cloned()
                        .ok_or_else(|| ProxyFailure::new(unknown_target_host(&host, many))),
                    None => Err(ProxyFailure::new(invalid_target_host(host))),
                },
                None if upstream.alias_derived => Err(ProxyFailure::new(missing_target_host(
                    many,
                    &upstream.alias,
                ))),
                None => Ok(round_robin(many, &self.round_robin)),
            },
        }?;
        // DESIGN §4.2: which endpoint a request landed on, and whether the
        // caller steered it there.
        self.metrics.record_endpoint_selected(
            &upstream.id.to_string(),
            &selected.host,
            match requested {
                Some(_) => crate::domain::ports::metrics::SelectionMethod::ExplicitHeader,
                None if endpoints.len() > 1 => {
                    crate::domain::ports::metrics::SelectionMethod::RoundRobin
                }
                None => crate::domain::ports::metrics::SelectionMethod::Default,
            },
        );
        if requested.is_some() {
            self.metrics
                .record_target_host_used(&upstream.id.to_string(), &selected.host);
        }
        Ok(selected)
    }

    /// SSRF and transport-security checks on the selected endpoint.
    fn check_target(&self, endpoint: &Endpoint, upstream_id: Uuid) -> DataPlaneResult<()> {
        let ssrf = &self.config.ssrf_policy;
        if ssrf.enabled && !ssrf.allow_private_addresses && is_private_host(&endpoint.host) {
            return Err(ProxyFailure::new(OagwError::Validation(format!(
                "target host '{}' is not reachable under the SSRF policy",
                endpoint.host
            )))
            .with_upstream(upstream_id));
        }
        if endpoint.scheme == "http" && !self.config.allow_http_upstream {
            return Err(ProxyFailure::new(OagwError::ProtocolError(
                "plain-http upstreams are disabled; configure an https endpoint".to_owned(),
            ))
            .with_upstream(upstream_id)
            .with_host(endpoint.host.clone()));
        }
        Ok(())
    }

    /// Fail fast while the breaker is open.
    fn check_breaker(&self, upstream_id: Uuid, host: &str) -> DataPlaneResult<()> {
        if self.breakers.check(upstream_id, host) == BreakerCheck::Open {
            return Err(ProxyFailure::new(breaker_open())
                .with_upstream(upstream_id)
                .with_host(host));
        }
        Ok(())
    }

    /// Send the upstream request.
    async fn send(
        &self,
        method: &str,
        url: &str,
        headers: axum::http::HeaderMap,
        body: Bytes,
    ) -> DataPlaneResult<HttpResponse> {
        let builder = builder_for(&self.http, method, url)
            .headers(headers_to_pairs(&headers))
            .body_bytes(body);
        builder
            .send()
            .await
            .map_err(OagwError::from)
            .map_err(ProxyFailure::new)
    }

    /// Build the buffered response, running the response-side plugin phases.
    async fn buffered_response(
        &self,
        upstream: &Upstream,
        plugins: &crate::domain::model::PluginsConfig,
        rules: &HeadersConfig,
        response: HttpResponse,
        context: &mut RequestContext,
    ) -> DataPlaneResult<ProxyOutcome> {
        let (status, headers, body) = split_response(response).await?;
        let mut response_context = ResponseContext {
            request_headers: context.headers.clone(),
            headers,
            status,
            body,
            config: serde_json::Map::new(),
        };
        for binding in plugin_bindings(plugins) {
            if self.plugins.missing(&binding.plugin_ref) {
                return Err(ProxyFailure::new(OagwError::PluginNotFound(format!(
                    "plugin '{}' has no implementation",
                    binding.plugin_ref
                )))
                .with_upstream(upstream.id));
            }
            context.config = binding.config.as_object().cloned().unwrap_or_default();
            response_context.config = context.config.clone();
            if let Some(guard) = self.plugins.guard_plugin(&binding) {
                let decision = guard.guard_response(&response_context).await;
                reject_or_allow(decision, upstream.id)?;
            }
            if let Some(transform) = self.plugins.transform_plugin(&binding) {
                transform
                    .transform_response(&mut response_context)
                    .await
                    .map_err(|error| {
                        ProxyFailure::new(OagwError::from(error)).with_upstream(upstream.id)
                    })?;
            }
        }
        let mut headers = response_context.headers;
        strip_hop_by_hop(&mut headers);
        apply_response_rules(&mut headers, &rules.response);
        set_source(&mut headers, source_for(status));
        Ok(ProxyOutcome {
            status,
            headers,
            body: ProxyBody::Full(response_context.body),
            upstream_id: upstream.id,
            host: String::new(),
        })
    }

    /// Build the streaming (SSE) response.
    async fn streaming_response(
        &self,
        upstream: &Upstream,
        response: HttpResponse,
    ) -> DataPlaneResult<ProxyOutcome> {
        let inner = response.into_inner();
        let status = inner.status();
        let mut headers = inner.headers().clone();
        strip_hop_by_hop(&mut headers);
        // The stream is relayed from the upstream, so its headers attribute it
        // to the upstream; a mid-stream abort is reported through an `error`
        // event inside the body, because the status line is already on the wire.
        set_source(&mut headers, "upstream");
        let stream = body_stream(inner.into_body());
        Ok(ProxyOutcome {
            status,
            headers,
            body: ProxyBody::Stream(Box::pin(stream)),
            upstream_id: upstream.id,
            host: String::new(),
        })
    }

    // ---------------------------------------------------------------------
    // WebSocket leg
    // ---------------------------------------------------------------------

    /// Resolve the upstream target for a WebSocket upgrade.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyFailure`] for alias, route, auth, rate-limit and
    /// transport-policy errors.
    pub async fn resolve_for_websocket(
        &self,
        mut request: ProxyRequest,
    ) -> DataPlaneResult<WebSocketLeg> {
        let resolved = self.resolve(&mut request).await?;
        let upstream = resolved.upstream.clone();
        let effective = resolved.effective.clone();
        // The handshake is a proxied request like any other, so it answers to
        // the same policy phases before the upgrade (contract §5): CORS on the
        // actual request, then auth, rate limit, guards and transforms.
        if let Some(error) = cors_error(resolved.effective.cors.as_ref(), &request) {
            return Err(ProxyFailure::new(error).with_upstream(upstream.id));
        }
        let mut context = self.request_context(&request, &upstream);
        // The allowlist governs what the caller sent; plugins may append to the
        // filtered query afterwards and are not subject to it.
        context.query = filtered_query(&resolved.route, request.query.as_deref());
        if let Some(binding) = effective.auth.as_ref().map(auth_binding) {
            self.authenticate(&mut context, &binding, upstream.id)
                .await?;
        }
        self.enforce_rate_limit(
            effective.rate_limit.as_ref(),
            effective.rate_limit_owner.unwrap_or(upstream.id),
            &request,
            upstream.id,
        )?;
        self.run_request_plugins(&mut context, &effective.plugins, upstream.id, true)
            .await?;
        let endpoint = self.select_endpoint(&upstream, request.target_host())?;
        self.check_target(&endpoint, upstream.id)?;
        let path = target_path(&resolved.route, &resolved.remainder)?;
        let url = ws_url(&endpoint, &path, context.query.as_deref())?;
        let host = endpoint.host.clone();
        self.check_breaker(upstream.id, &host)?;
        Ok(WebSocketLeg {
            url,
            headers: headers_to_pairs(&outbound_headers(
                &context.headers,
                &effective.headers.request,
                &request.headers,
            )),
            upstream_id: upstream.id,
            host,
        })
    }

    /// Record a successful WebSocket leg.
    pub fn websocket_success(&self, leg: &WebSocketLeg) {
        self.breaker_success(leg.upstream_id, &leg.host);
    }

    /// Record a failed WebSocket leg.
    pub fn websocket_failure(&self, leg: &WebSocketLeg) {
        self.breaker_failure(leg.upstream_id, &leg.host);
    }

    /// Close the breaker for an endpoint and report the transition (DESIGN §4.2).
    fn breaker_success(&self, upstream_id: Uuid, host: &str) {
        if let Some(to) = self.breakers.record_success(upstream_id, host) {
            self.metrics.set_breaker_state(host, to);
        }
    }

    /// Trip the breaker for an endpoint and report the transition (DESIGN §4.2).
    fn breaker_failure(&self, upstream_id: Uuid, host: &str) {
        if let Some(to) = self.breakers.record_failure(upstream_id, host) {
            self.metrics.set_breaker_state(host, to);
            // Only two states exist, so the state the breaker left is its
            // opposite.
            let from = match to {
                crate::domain::ports::metrics::BreakerState::Closed => {
                    crate::domain::ports::metrics::BreakerState::Open
                }
                crate::domain::ports::metrics::BreakerState::Open => {
                    crate::domain::ports::metrics::BreakerState::Closed
                }
            };
            self.metrics.record_breaker_transition(host, from, to);
        }
    }
}

// ---------------------------------------------------------------------------------------
// free helpers
// ---------------------------------------------------------------------------------------

fn unknown_target_host(host: &str, endpoints: &[Endpoint]) -> OagwError {
    OagwError::UnknownTargetHost {
        invalid_value: host.to_owned(),
        valid_hosts: endpoint_hosts(endpoints),
    }
}

/// ADR-0007: `X-OAGW-Target-Host` must be a bare hostname or IP address — no
/// scheme, port, path or other special characters. Returns the canonical
/// lowercase host, or `None` when the value is malformed and must be reported
/// as `invalid_target_host.v1` rather than looked up.
#[must_use]
pub fn normalized_target_host(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.contains(['/', '?', '#', '@', ' ', '\t'])
        || trimmed.contains("://")
    {
        return None;
    }
    if let Ok(ip) = std::net::IpAddr::from_str(trimmed) {
        return Some(ip.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let (inside, tail) = rest.split_once(']')?;
        if !tail.is_empty() {
            // `[::1]:8080` — a port is not a valid target host (ADR-0007).
            return None;
        }
        return std::net::IpAddr::from_str(inside)
            .ok()
            .map(|ip| ip.to_string());
    }
    // Anything else carrying a colon is a `host:port` pair, which the header
    // must not contain.
    if trimmed.contains(':') {
        return None;
    }
    let host = trimmed.trim_end_matches('.').to_ascii_lowercase();
    if !is_valid_hostname(&host) {
        return None;
    }
    Some(host)
}

/// RFC 1123 hostname: dot-separated labels of letters, digits and hyphens that
/// neither start nor end with a hyphen, each at most 63 bytes, 253 in total.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

fn invalid_target_host(host: &str) -> OagwError {
    OagwError::InvalidTargetHost {
        invalid_value: host.to_owned(),
    }
}

fn missing_target_host(endpoints: &[Endpoint], alias: &str) -> OagwError {
    OagwError::MissingTargetHost {
        valid_hosts: endpoint_hosts(endpoints),
        alias: alias.to_owned(),
    }
}

fn endpoint_hosts(endpoints: &[Endpoint]) -> Vec<String> {
    endpoints.iter().map(Endpoint::normalized_host).collect()
}

/// `X-OAGW-Error-Source` value for a response that was produced by the upstream.
///
/// ADR-0007: the header names the origin of the response the caller received, so
/// anything relayed from the upstream — a 200 as much as its errors — is
/// attributed to `upstream`, while every response the gateway generates itself
/// (problems, the CORS preflight) is attributed to `gateway`.
#[must_use]
pub fn source_for(_status: axum::http::StatusCode) -> &'static str {
    "upstream"
}

fn set_source(headers: &mut axum::http::HeaderMap, value: &str) {
    if let Some(parsed) = crate::infra::proxy::headers::header_value(value) {
        headers.insert(
            axum::http::HeaderName::from_static(ERROR_SOURCE_HEADER),
            parsed,
        );
    }
}

/// Attach the `X-RateLimit-*` counters of an accepted request.
fn set_rate_limit_headers(headers: &mut axum::http::HeaderMap, decision: &RateDecision) {
    let pairs = [
        ("x-ratelimit-limit", decision.limit.to_string()),
        ("x-ratelimit-remaining", decision.remaining.to_string()),
        ("x-ratelimit-reset", decision.reset_epoch.to_string()),
    ];
    for (name, value) in pairs {
        let Some(parsed) = crate::infra::proxy::headers::header_value(&value) else {
            continue;
        };
        if let Ok(name) = axum::http::HeaderName::from_bytes(name.as_bytes()) {
            headers.insert(name, parsed);
        }
    }
}

/// Request path used for matching, always starting with `/`.
#[must_use]
pub fn normalized_request_path(suffix: &str) -> String {
    let trimmed = suffix.trim_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        format!("/{trimmed}")
    }
}

/// Longest matching route plus the path remainder after the matched prefix.
///
/// # Errors
///
/// Returns [`OagwError::RouteNotFound`] when no route matches.
pub fn match_route(
    routes: &[Route],
    method: &str,
    request_path: &str,
) -> Result<(Route, String), OagwError> {
    let mut best: Option<(usize, Route, String)> = None;
    for route in routes {
        if !route.spec.enabled {
            continue;
        }
        let Some(http) = route.spec.match_config.http.as_ref() else {
            continue;
        };
        if !http
            .methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(method))
        {
            continue;
        }
        let Some(remainder) = segment_prefix(&http.path, request_path) else {
            continue;
        };
        let depth = segment_depth(&http.path);
        if best
            .as_ref()
            .is_none_or(|(best_depth, _, _)| depth > *best_depth)
        {
            best = Some((depth, route.clone(), remainder));
        }
    }
    best.map(|(_, route, remainder)| (route, remainder))
        .ok_or_else(|| OagwError::RouteNotFound(NO_ROUTE.to_owned()))
}

fn segment_depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

/// Path-only route match for CORS preflights, whose `OPTIONS` method is not
/// part of the route's allowlist.
fn match_preflight_route(
    routes: &[Route],
    request_path: &str,
) -> Result<(Route, String), OagwError> {
    let mut best: Option<(usize, Route, String)> = None;
    for route in routes {
        if !route.spec.enabled {
            continue;
        }
        let Some(http) = route.spec.match_config.http.as_ref() else {
            continue;
        };
        let Some(remainder) = segment_prefix(&http.path, request_path) else {
            continue;
        };
        let depth = segment_depth(&http.path);
        if best
            .as_ref()
            .is_none_or(|(best_depth, _, _)| depth > *best_depth)
        {
            best = Some((depth, route.clone(), remainder));
        }
    }
    best.map(|(_, route, remainder)| (route, remainder))
        .ok_or_else(|| OagwError::RouteNotFound(NO_ROUTE.to_owned()))
}

/// Segment-wise prefix match, returning the remainder without a leading slash.
#[must_use]
pub fn segment_prefix(prefix: &str, path: &str) -> Option<String> {
    let prefix_parts: Vec<&str> = prefix.split('/').filter(|part| !part.is_empty()).collect();
    let path_parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if prefix_parts.len() > path_parts.len() {
        return None;
    }
    for (index, part) in prefix_parts.iter().enumerate() {
        if !part.eq_ignore_ascii_case(path_parts[index]) {
            return None;
        }
    }
    Some(path_parts[prefix_parts.len()..].join("/"))
}

/// Upstream target path for the matched route.
///
/// # Errors
///
/// Returns [`OagwError::RouteError`] when the route forbids a path suffix and
/// [`OagwError::ProtocolError`] for gRPC-only routes.
pub fn target_path(route: &Route, remainder: &str) -> Result<String, OagwError> {
    let Some(http) = route.spec.match_config.http.as_ref() else {
        return Err(OagwError::ProtocolError(
            "gRPC routes are not proxied by the MVP data plane".to_owned(),
        ));
    };
    if remainder.is_empty() {
        return Ok(collapse_slashes(&http.path));
    }
    if http.path_suffix_mode == PathSuffixMode::Disabled {
        return Err(OagwError::RouteError(
            "path suffix is not accepted by this route".to_owned(),
        ));
    }
    Ok(collapse_slashes(&format!("{}/{remainder}", http.path)))
}

/// Collapse to a single leading slash and drop trailing separators.
#[must_use]
pub fn collapse_slashes(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        format!("/{trimmed}")
    }
}

/// Query string restricted to the route's allowlist.
#[must_use]
pub fn filtered_query(route: &Route, raw: Option<&str>) -> Option<String> {
    let allowlist = &route.spec.match_config.http.as_ref()?.query_allowlist;
    let raw = raw?;
    if allowlist.is_empty() {
        return None;
    }
    let pairs: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
        .filter(|(name, _)| {
            allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name))
        })
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    if pairs.is_empty() {
        return None;
    }
    Some(
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish(),
    )
}

/// Absolute upstream URL for the HTTP / SSE leg.
#[must_use]
pub fn target_url(endpoint: &Endpoint, path: &str, query: Option<&str>) -> String {
    match query.filter(|value| !value.is_empty()) {
        Some(value) => format!(
            "{}://{}:{}{}?{value}",
            endpoint.scheme, endpoint.host, endpoint.port, path
        ),
        None => format!(
            "{}://{}:{}{}",
            endpoint.scheme, endpoint.host, endpoint.port, path
        ),
    }
}

/// Absolute upstream URL for the WebSocket leg.
///
/// # Errors
///
/// Returns [`OagwError::LinkUnavailable`] for `wt://` endpoints, which have no
/// HTTP-3 fallback in the MVP data plane.
pub fn ws_url(endpoint: &Endpoint, path: &str, query: Option<&str>) -> Result<String, OagwError> {
    match endpoint.scheme.as_str() {
        // `wt` has no HTTP fallback in the MVP data plane.
        "wt" => Err(OagwError::LinkUnavailable(
            "webtransport endpoints have no http fallback for websocket upgrades".to_owned(),
        )),
        // A WebSocket handshake is only speakable over `ws` / `wss`: the HTTP
        // scheme of the endpoint has to be rewritten to its WebSocket form.
        "https" | "wss" => Ok(ws_target(endpoint, "wss", path, query)),
        "grpc" | "grpcs" => Err(OagwError::LinkUnavailable(
            "grpc endpoints do not accept websocket upgrades".to_owned(),
        )),
        _ => Ok(ws_target(endpoint, "ws", path, query)),
    }
}

/// `scheme://host:port/path` for the WebSocket leg.
fn ws_target(endpoint: &Endpoint, scheme: &str, path: &str, query: Option<&str>) -> String {
    let plain = Endpoint {
        scheme: scheme.to_owned(),
        ..endpoint.clone()
    };
    target_url(&plain, path, query)
}

/// Round-robin over the endpoint pool.
fn round_robin(endpoints: &[Endpoint], counter: &AtomicU64) -> Endpoint {
    let index = counter.fetch_add(1, Ordering::Relaxed);
    let len = u64::try_from(endpoints.len()).unwrap_or(1).max(1);
    endpoints
        .get(usize::try_from(index % len).unwrap_or_default())
        .cloned()
        .unwrap_or_else(|| endpoints[0].clone())
}

/// SSRF guard: reject loopback, link-local, private and unique-local literals.
///
/// Names are not resolved here (the gateway has no DNS resolver of its own in
/// the MVP), so a host that is *not* an address literal is only refused when it
/// is the well-known `localhost` name; the DNS-result validation the PRD asks
/// for happens on the resolved address inside the HTTP client transport.
#[must_use]
pub fn is_private_host(host: &str) -> bool {
    let candidate = bare_host(host);
    if candidate.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let Ok(address) = std::net::IpAddr::from_str(&candidate) else {
        return false;
    };
    match address {
        std::net::IpAddr::V4(value) => {
            value.is_loopback()
                || value.is_private()
                || value.is_link_local()
                || value.is_broadcast()
                || value.is_multicast()
                || value.is_unspecified()
                || value.is_documentation()
        }
        std::net::IpAddr::V6(value) => {
            value.is_loopback()
                || value.is_multicast()
                || value.is_unspecified()
                || value.is_unique_local()
                || value.is_unicast_link_local()
                || mapped_v4(value).is_some_and(is_private_host_v4)
        }
    }
}

/// The IPv4 address behind an IPv4-mapped IPv6 literal, if there is one.
fn mapped_v4(value: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let octets = value.octets();
    (octets[..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff])
        .then(|| std::net::Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]))
}

/// The IPv4 half of the guard, shared with IPv4-mapped IPv6 literals.
fn is_private_host_v4(value: std::net::Ipv4Addr) -> bool {
    value.is_loopback()
        || value.is_private()
        || value.is_link_local()
        || value.is_broadcast()
        || value.is_multicast()
        || value.is_unspecified()
        || value.is_documentation()
}

fn reject_or_allow(
    decision: Result<crate::domain::plugin::GuardDecision, PluginError>,
    upstream_id: Uuid,
) -> DataPlaneResult<()> {
    use crate::domain::plugin::GuardDecision;
    match decision {
        Ok(GuardDecision::Allow) => Ok(()),
        Ok(GuardDecision::Reject {
            status,
            error_code,
            message,
        }) => Err(ProxyFailure::new(OagwError::from(PluginError::Rejected {
            status,
            error_code,
            message,
        }))
        .with_upstream(upstream_id)),
        Err(error) => Err(ProxyFailure::new(OagwError::from(error)).with_upstream(upstream_id)),
    }
}

/// Names never forwarded upstream: hop-by-hop, routing and identity headers.
fn forward_skip(
    rules: &crate::domain::model::RequestHeaders,
    inbound: &axum::http::HeaderMap,
) -> Vec<&'static str> {
    let mut skip: Vec<&'static str> = vec![TARGET_HOST_HEADER, "host", "content-length"];
    skip.extend(HOP_BY_HOP);
    if !authorization_allowed(rules, inbound) {
        skip.push(AUTHORIZATION);
    }
    skip
}

fn authorization_allowed(
    rules: &crate::domain::model::RequestHeaders,
    inbound: &axum::http::HeaderMap,
) -> bool {
    if !inbound.contains_key(AUTHORIZATION) {
        return true;
    }
    match rules.passthrough {
        crate::domain::model::Passthrough::All => true,
        crate::domain::model::Passthrough::Allowlist => rules
            .passthrough_allowlist
            .iter()
            .any(|name| name.eq_ignore_ascii_case(AUTHORIZATION)),
        crate::domain::model::Passthrough::None => false,
    }
}

/// Plugin bindings for a plugin-chain configuration.
///
/// Each entry carries the configuration bound next to its reference
/// (ADR-0009), so a guard such as `required_headers.v1` can receive the
/// headers it has to enforce.
fn plugin_bindings(config: &crate::domain::model::PluginsConfig) -> Vec<PluginBinding> {
    config
        .items
        .iter()
        .map(|item| PluginBinding {
            plugin_ref: item.reference().to_owned(),
            config: item.config(),
        })
        .collect()
}

/// Compose the headers sent to the upstream.
///
/// The plugin chain runs over the caller's view of the request (ADR-0002), so
/// the outbound set is the rule-driven view of the caller's headers —
/// passthrough, set/add/remove and the `authorization` skip rule — overlaid
/// with whatever the chain itself changed: a credential the auth plugin
/// injected, or a header a transform added, travels even when `passthrough`
/// is `none`, while caller headers the chain left untouched still obey the
/// passthrough rule.
fn outbound_headers(
    chain_headers: &axum::http::HeaderMap,
    rules: &crate::domain::model::RequestHeaders,
    inbound: &axum::http::HeaderMap,
) -> axum::http::HeaderMap {
    let skip = forward_skip(rules, inbound);
    let mut out = build_request_headers(inbound, rules, &skip);
    for name in chain_headers.keys() {
        if skip
            .iter()
            .any(|skipped| name.as_str().eq_ignore_ascii_case(skipped))
        {
            continue;
        }
        let chain_values: Vec<&axum::http::HeaderValue> =
            chain_headers.get_all(name).iter().collect();
        let caller_values: Vec<&axum::http::HeaderValue> = inbound.get_all(name).iter().collect();
        if chain_values == caller_values {
            continue;
        }
        out.remove(name);
        for value in chain_values {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Turn the effective auth block into the plugin binding the registry resolves.
fn auth_binding(auth: &crate::domain::model::AuthConfig) -> PluginBinding {
    PluginBinding {
        plugin_ref: auth.plugin_type.clone(),
        config: serde_json::Value::Object(auth.config.clone()),
    }
}

/// Headers serialised for the outbound request builder.
fn headers_to_pairs(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let text = value.to_str().ok()?;
            Some((name.as_str().to_owned(), text.to_owned()))
        })
        .collect()
}

/// Pick the request-builder preset that carries the same method.
///
/// `toolkit_http` exposes fixed-verb builders only; the route match restricts
/// the data plane to `GET`/`POST`/`PUT`/`DELETE`/`PATCH`, so the mapping is
/// total for reachable requests.
fn builder_for(http: &HttpClient, method: &str, url: &str) -> toolkit_http::RequestBuilder {
    match method {
        "POST" => http.post(url),
        "PUT" => http.put(url),
        "DELETE" => http.delete(url),
        "PATCH" => http.patch(url),
        _ => http.get(url),
    }
}

/// `true` when either side asked for an event stream.
#[must_use]
pub fn is_streaming(response_headers: &axum::http::HeaderMap) -> bool {
    response_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(SSE_MEDIA_TYPE))
}

/// `true` for a CORS preflight request.
#[must_use]
pub fn preflight_requested(request: &ProxyRequest) -> bool {
    request.method == "OPTIONS"
        && request.headers.contains_key(axum::http::header::ORIGIN)
        && request
            .headers
            .contains_key(axum::http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

fn request_header<'a>(request: &'a ProxyRequest, name: &str) -> Option<&'a str> {
    let name = axum::http::HeaderName::from_bytes(name.as_bytes()).ok()?;
    request
        .headers
        .get(&name)
        .and_then(|value| value.to_str().ok())
}

/// Build the permissive 204 preflight response.
///
/// ADR-0004: the preflight echoes whatever the browser asked for; origin and
/// method enforcement happens on the actual request that follows it, once the
/// upstream — and therefore its CORS configuration — has been resolved.
fn preflight_response(request: &ProxyRequest) -> ProxyOutcome {
    let origin = request_header(request, "origin").unwrap_or("*");
    let requested_method = request_header(request, "access-control-request-method").unwrap_or("");
    let requested_headers = request_header(request, "access-control-request-headers").unwrap_or("");
    let mut headers = axum::http::HeaderMap::new();
    let values: [(&str, &str); 5] = [
        ("access-control-allow-origin", origin),
        ("access-control-allow-methods", requested_method),
        ("access-control-max-age", "86400"),
        ("access-control-allow-headers", requested_headers),
        (
            "vary",
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    ];
    for (name, value) in values {
        if let (Some(name), Some(value)) = (
            crate::infra::proxy::headers::header_name(name),
            crate::infra::proxy::headers::header_value(value),
        ) {
            headers.insert(name, value);
        }
    }
    // `Access-Control-Allow-Credentials` is deliberately absent: ADR-0004's
    // preflight header list does not include it, and granting it before the
    // origin has been validated would let any origin preflight credentialed
    // access. The actual request sets it once the CORS config is resolved.
    set_source(&mut headers, "gateway");
    ProxyOutcome {
        status: axum::http::StatusCode::NO_CONTENT,
        headers,
        body: ProxyBody::Full(Bytes::new()),
        upstream_id: Uuid::nil(),
        host: String::new(),
    }
}

fn cors_origin_allowed(cors: &CorsConfig, origin: &str) -> bool {
    cors.allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

fn cors_method_allowed(cors: &CorsConfig, method: &str) -> bool {
    cors.methods()
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method))
}

/// `403` when the origin or the method is outside the effective allowlist.
fn cors_error(cors: Option<&CorsConfig>, request: &ProxyRequest) -> Option<OagwError> {
    let cors = cors?;
    let origin = request_header(request, "origin")?;
    if !cors_origin_allowed(cors, origin) {
        return Some(OagwError::CorsOriginNotAllowed(
            "origin is not allowed by the effective CORS configuration".to_owned(),
        ));
    }
    if !cors_method_allowed(cors, &request.method) {
        return Some(OagwError::CorsMethodNotAllowed(
            "method is not allowed by the effective CORS configuration".to_owned(),
        ));
    }
    None
}

/// Attach the CORS headers of an accepted **actual** response (ADR-0004):
/// the caller's origin, the credential flag, the exposed headers and
/// `Vary: Origin` so caches never serve one origin's response to another.
fn set_cors_response_headers(headers: &mut axum::http::HeaderMap, cors: &CorsConfig, origin: &str) {
    let mut pairs: Vec<(&str, String)> = vec![("access-control-allow-origin", origin.to_owned())];
    if cors.allow_credentials {
        pairs.push(("access-control-allow-credentials", "true".to_owned()));
    }
    if !cors.expose_headers.is_empty() {
        pairs.push((
            "access-control-expose-headers",
            cors.expose_headers.join(", "),
        ));
    }
    for (name, value) in pairs {
        let (Ok(name), Some(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            crate::infra::proxy::headers::header_value(&value),
        ) else {
            continue;
        };
        headers.insert(name, value);
    }
    if !headers.contains_key(axum::http::header::VARY)
        && let Some(value) = crate::infra::proxy::headers::header_value("Origin")
    {
        headers.insert(axum::http::header::VARY, value);
    }
}

/// Apply the matched route over the chain-merged effective configuration.
#[must_use]
pub fn with_route(effective: &EffectiveConfig, route: &Route) -> EffectiveConfig {
    let mut merged = effective.clone();
    // Route plugins always append to the upstream chain (DESIGN §3.3):
    // `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`. The `sharing` gate governs
    // tenant inheritance only, and a route is not a tenant-chain participant.
    {
        let mut items = merged.plugins.items;
        for item in &route
            .spec
            .plugins
            .as_ref()
            .map_or_else(Vec::new, |p| p.items.clone())
        {
            // A route may rebind a plugin the upstream already runs; its
            // configuration (and binding order) then replaces the upstream's.
            match items
                .iter_mut()
                .find(|existing| existing.reference() == item.reference())
            {
                Some(existing) => *existing = item.clone(),
                None => items.push(item.clone()),
            }
        }
        merged.plugins.items = items;
    }
    let route_limit = route.spec.rate_limit.clone();
    if route_limit.is_some() {
        // A route limit is its own budget: the configuring resource is the
        // route, not the upstream whose limit it overrides (ADR-0003).
        merged.rate_limit_owner = Some(route.id);
    }
    merged.rate_limit =
        crate::domain::merge::merge_route_rate_limit(merged.rate_limit.clone(), route_limit);
    merged.cors = crate::domain::merge::merge_cors(merged.cors.clone(), route.spec.cors.clone());
    for tag in &route.spec.tags {
        if !merged.tags.contains(tag) {
            merged.tags.push(tag.clone());
        }
    }
    if !route.spec.enabled {
        merged.enabled = false;
    }
    merged
}

/// Next body frame, mapping a transport failure onto
/// [`OagwError::StreamAborted`] (`None` ends the stream).
async fn next_frame(
    body: &mut toolkit_http::ResponseBody,
) -> Option<Result<Frame<Bytes>, OagwError>> {
    let frame =
        futures_util::future::poll_fn(|cx| HttpBody::poll_frame(std::pin::Pin::new(body), cx))
            .await;
    match frame {
        Some(Ok(frame)) => Some(Ok(frame)),
        Some(Err(_)) => Some(Err(OagwError::StreamAborted(
            "upstream stream aborted".to_owned(),
        ))),
        None => None,
    }
}

/// Split an upstream response into status, headers and buffered body.
/// Buffer an upstream response body, refusing bodies over [`MAX_BODY_BYTES`].
///
/// The same hard limit the inbound request honours (DESIGN
/// `cpt-cf-oagw-constraint-body-limit`): an unbounded upstream reply would let
/// a misbehaving peer exhaust the gateway's memory.
async fn split_response(
    response: HttpResponse,
) -> DataPlaneResult<(axum::http::StatusCode, axum::http::HeaderMap, Bytes)> {
    let inner = response.into_inner();
    let status = inner.status();
    let headers = inner.headers().clone();
    let mut body = inner.into_body();
    let mut collected: Vec<u8> = Vec::new();
    loop {
        let Some(frame) = next_frame(&mut body).await else {
            break;
        };
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            if collected.len().saturating_add(data.len()) > MAX_BODY_BYTES {
                return Err(ProxyFailure::new(OagwError::PayloadTooLarge));
            }
            collected.extend_from_slice(data);
        }
    }
    Ok((status, headers, Bytes::from(collected)))
}

/// Turn an upstream body into a stream of chunks, mapping transport failures
/// onto [`OagwError::StreamAborted`].
fn body_stream(
    body: toolkit_http::ResponseBody,
) -> impl Stream<Item = Result<Bytes, OagwError>> + Send {
    futures_util::stream::try_unfold(body, |mut body| async move {
        loop {
            match next_frame(&mut body).await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        return Ok(Some((data.clone(), body)));
                    }
                }
                Some(Err(error)) => return Err(error),
                None => return Ok(None),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::model::{
        Endpoint, HttpMatch, MatchConfig, RouteCreate, ServerConfig, Upstream, UpstreamCreate,
        normalize_host,
    };

    fn endpoint(host: &str) -> Endpoint {
        Endpoint {
            scheme: "https".to_owned(),
            host: host.to_owned(),
            port: 443,
        }
    }

    fn route(path: &str, methods: &[&str]) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            spec: RouteCreate {
                tags: Vec::new(),
                upstream_id: Uuid::new_v4(),
                enabled: true,
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
            },
        }
    }

    #[test]
    fn request_path_is_normalized() {
        assert_eq!(normalized_request_path(""), "/");
        assert_eq!(normalized_request_path("api/users"), "/api/users");
        assert_eq!(normalized_request_path("/api/"), "/api");
    }

    #[test]
    fn segment_prefix_returns_remainder() {
        assert_eq!(
            segment_prefix("/api", "/api/users/1").as_deref(),
            Some("users/1")
        );
        assert_eq!(
            segment_prefix("/", "/api/users").as_deref(),
            Some("api/users")
        );
        assert!(segment_prefix("/api", "/other").is_none());
        assert!(segment_prefix("/api/x", "/api").is_none());
    }

    #[test]
    fn picks_the_longest_enabled_route_for_the_method() {
        let root = route("/", &["GET"]);
        let nested = route("/api", &["GET"]);
        let other_method = route("/api/users", &["POST"]);
        let disabled = {
            let mut r = route("/api/users/1", &["GET"]);
            r.spec.enabled = false;
            r
        };
        let routes = vec![root, nested, other_method, disabled];
        let (matched, remainder) = match_route(&routes, "GET", "/api/users/1").expect("match");
        assert_eq!(matched.spec.match_config.http.expect("http").path, "/api");
        assert_eq!(remainder, "users/1");
    }

    #[test]
    fn no_route_is_a_404() {
        let routes = vec![route("/api", &["POST"])];
        assert!(match_route(&routes, "GET", "/api").is_err());
    }

    #[test]
    fn disabled_route_falls_back_to_the_root_route() {
        let root = route("/", &["GET"]);
        let mut nested = route("/api", &["GET"]);
        nested.spec.enabled = false;
        let (matched, _) = match_route(&[root, nested], "GET", "/api/x").expect("match");
        assert_eq!(matched.spec.match_config.http.expect("http").path, "/");
    }

    #[test]
    fn target_path_appends_the_suffix() {
        let r = route("/api", &["GET"]);
        assert_eq!(target_path(&r, "users/1").expect("path"), "/api/users/1");
        assert_eq!(target_path(&r, "").expect("path"), "/api");
    }

    #[test]
    fn target_path_rejects_suffix_in_disabled_mode() {
        let mut r = route("/api", &["GET"]);
        r.spec
            .match_config
            .http
            .as_mut()
            .expect("http")
            .path_suffix_mode = PathSuffixMode::Disabled;
        assert!(target_path(&r, "users").is_err());
        assert!(target_path(&r, "").is_ok());
    }

    #[test]
    fn query_is_filtered_by_the_allowlist() {
        let mut r = route("/api", &["GET"]);
        r.spec
            .match_config
            .http
            .as_mut()
            .expect("http")
            .query_allowlist = vec!["page".to_owned()];
        assert_eq!(
            filtered_query(&r, Some("page=2&secret=1")).as_deref(),
            Some("page=2")
        );
        assert!(filtered_query(&r, Some("secret=1")).is_none());
        assert!(filtered_query(&r, None).is_none());
        let mut empty = route("/api", &["GET"]);
        empty
            .spec
            .match_config
            .http
            .as_mut()
            .expect("http")
            .query_allowlist = Vec::new();
        assert!(filtered_query(&empty, Some("page=1")).is_none());
    }

    #[test]
    fn target_url_composes_scheme_host_port_path() {
        let e = endpoint("a.example.com");
        assert_eq!(
            target_url(&e, "/api", Some("x=1")),
            "https://a.example.com:443/api?x=1"
        );
        assert_eq!(
            target_url(&e, "/api", None),
            "https://a.example.com:443/api"
        );
    }

    #[test]
    fn source_header_tracks_the_origin_of_errors() {
        assert_eq!(source_for(axum::http::StatusCode::OK), "upstream");
        assert_eq!(source_for(axum::http::StatusCode::CREATED), "upstream");
        assert_eq!(source_for(axum::http::StatusCode::UNAUTHORIZED), "upstream");
        assert_eq!(
            source_for(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
            "upstream"
        );
    }

    #[test]
    fn streaming_is_detected_from_the_upstream_content_type() {
        // The caller's `accept` alone must not turn a buffered reply into a
        // stream: the response phases would be skipped for a body the upstream
        // delivered in full.
        assert!(!is_streaming(&axum::http::HeaderMap::new()));
        let mut accept_only = axum::http::HeaderMap::new();
        accept_only.insert(
            axum::http::header::ACCEPT,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        assert!(!is_streaming(&accept_only));
        let mut response = axum::http::HeaderMap::new();
        response.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_streaming(&response));
    }

    #[test]
    fn private_hosts_are_rejected_when_ssrf_is_enabled() {
        assert!(is_private_host("127.0.0.1"));
        assert!(is_private_host("10.0.0.5"));
        assert!(is_private_host("169.254.169.254"));
        assert!(is_private_host("localhost"));
        assert!(is_private_host("LOCALHOST"));
        assert!(is_private_host("::1"));
        assert!(is_private_host("fc00::1"));
        assert!(is_private_host("fd12:3456:789a::1"));
        assert!(is_private_host("fe80::1"));
        assert!(is_private_host("::ffff:10.0.0.5"));
        assert!(is_private_host("::ffff:169.254.169.254"));
        assert!(!is_private_host("api.example.com"));
        assert!(!is_private_host("8.8.8.8"));
        assert!(!is_private_host("2606:4700::1111"));
        assert!(!is_private_host("::ffff:8.8.8.8"));
    }

    #[test]
    fn preflight_is_detected_from_the_request_headers() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("https://app.example.com"),
        );
        headers.insert(
            axum::http::header::ACCESS_CONTROL_REQUEST_METHOD,
            axum::http::HeaderValue::from_static("GET"),
        );
        let request = ProxyRequest {
            method: "OPTIONS".to_owned(),
            alias: "a.example.com".to_owned(),
            path_suffix: String::new(),
            query: None,
            headers,
            body: Bytes::new(),
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            client_ip: String::new(),
            instance: "/".to_owned(),
            trace_id: "trace".to_owned(),
            security: SecurityContext::anonymous(),
            route_pattern: None,
        };
        assert!(preflight_requested(&request));
        let request = ProxyRequest {
            method: "GET".to_owned(),
            ..request
        };
        assert!(!preflight_requested(&request));
    }

    #[test]
    fn upstream_is_normalized_for_comparison() {
        assert_eq!(normalize_host("API.Example.COM."), "api.example.com");
    }

    #[test]
    fn server_config_helpers_exist() {
        let server = ServerConfig {
            endpoints: vec![endpoint("a.example.com"), endpoint("b.example.com")],
        };
        assert_eq!(server.endpoints.len(), 2);
        assert_eq!(endpoint_hosts(&server.endpoints).len(), 2);
    }

    #[test]
    fn upstream_alias_is_normalized() {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: "a.example.com".to_owned(),
            alias_derived: true,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            spec: UpstreamCreate {
                enabled: true,
                alias: None,
                tags: Vec::new(),
                server: ServerConfig {
                    endpoints: vec![endpoint("a.example.com")],
                },
                protocol: crate::domain::model::PROTOCOL_HTTP.to_owned(),
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        };
        assert!(upstream.alias_derived);
    }
}
