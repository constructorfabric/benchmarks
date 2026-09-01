//! Outbound proxy engine (DESIGN §3.5 "Proxy Request Flow").
//!
//! [`HttpProxyEngine`] folds the resolved tenant chain, the shadowing alias
//! resolution, the matched route and the plugin chain into one outbound call on
//! every request: no "effective configuration" is ever stored (ADR 0006
//! "State Management"). The pipeline is
//!
//! ```text
//! chain → alias → merge → route → CORS → endpoint → body → headers
//!       → auth → guards → transforms → rate limit → upstream
//!       → guards(response) → transforms(response) → response headers
//! ```
//!
//! The engine never retries (DESIGN §2.1 "No automatic retries") and never
//! caches an upstream response; a failed proxy call is reported as-is.
//!
//! Review evidence (privilege boundary — data plane):
//! * Guardrail: DESIGN §3.1 "Alias Resolution" + §4.4 "Security Considerations",
//!   ADR 0001 "Request Routing", ADR 0007 "Error Source Distinction".
//! * Rationale: the tenant chain is the only visibility filter consulted, the
//!   `X-OAGW-Error-Source` header is set on *every* response and request data
//!   (bodies, query strings, header values) is never written to a log line.
//! * Validation performed: `engine_*` tests drive the pipeline against real
//!   upstream servers (`httpmock`, a hand-rolled SSE TCP server and a
//!   `tokio-tungstenite` WebSocket peer).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use hyper::body::Incoming;
use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::domain::alias;
use crate::domain::body::validate_request_body;
use crate::domain::cors::{apply_response_headers, check_actual_request, classify, CorsRequest};
use crate::domain::error::DomainError;
use crate::domain::headers::{build_outbound_request_headers, build_outbound_response_headers};
use crate::domain::merge::{apply_route, merge_upstream_chain, EffectiveConfig};
use crate::domain::models::{
    Endpoint, HttpMatch, PathSuffixMode, RateLimitConfig, RateLimitScope, Route, Upstream,
};
use crate::domain::plugin::{GuardDecision, PluginRegistry, RequestContext, ResponseContext};
use crate::domain::rate_limit::{RateLimitDecision, RateLimitParameters};
use crate::domain::routing::{guard_route_match, normalize_method, resolve_alias, resolve_route};
use crate::domain::service::ControlPlaneService;
use crate::domain::target::{select_endpoint, SelectionMethod, TARGET_HOST_HEADER};
use crate::domain::time::now_millis;
use crate::infra::metrics::MetricsRegistry;
use crate::infra::ratelimit::{LimiterKey, LimiterRegistry};
use crate::infra::tenant::TenantChainResolver;
use crate::infra::transport::{
    ensure_allowed_scheme, request_target, Transport, UpstreamRequest, UpstreamResponse,
    OUTBOUND_QUERY_HEADER,
};

/// Header carrying the error source of every response (ADR 0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Header value for a response the gateway produced itself.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Header value for a response produced by the upstream (success or error).
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";
/// Response header publishing the tokens left in the bucket (ADR 0003).
pub const RATE_LIMIT_REMAINING_HEADER: &str = "x-ratelimit-remaining";
/// Response header publishing the bucket capacity (ADR 0003).
pub const RATE_LIMIT_LIMIT_HEADER: &str = "x-ratelimit-limit";
/// Response header publishing the window length in seconds (ADR 0003).
pub const RATE_LIMIT_RESET_HEADER: &str = "x-ratelimit-reset";
/// `http.route` label used when no route matched, keeping cardinality bounded.
pub const UNMATCHED_ROUTE: &str = "unmatched";

/// Which side produced a proxied response (ADR 0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// The gateway generated the response (problem document).
    Gateway,
    /// The upstream produced the response, passed through unchanged.
    Upstream,
}

impl ErrorSource {
    /// Wire value of the [`ERROR_SOURCE_HEADER`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => ERROR_SOURCE_GATEWAY,
            Self::Upstream => ERROR_SOURCE_UPSTREAM,
        }
    }
}

/// Inbound proxy request handed to [`HttpProxyEngine::execute`].
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// Alias the request was addressed to (`{alias}` of the proxy route).
    pub alias: String,
    /// HTTP method of the inbound request.
    pub method: String,
    /// Path below the alias, `/` when the request ended at the alias.
    pub path: String,
    /// Raw query string, without the leading `?`.
    pub query: Option<String>,
    /// Inbound headers, in arrival order.
    pub headers: Vec<(String, String)>,
    /// Buffered request payload.
    pub body: bytes::Bytes,
    /// Authenticated caller.
    pub security: SecurityContext,
}

impl ProxyRequest {
    /// Inbound header value, case-insensitive.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name_, _)| name_.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Successful result of a proxied request.
#[derive(Debug)]
pub struct ProxyOutcome {
    /// Status returned by the upstream.
    pub status: u16,
    /// Headers to return to the caller, in order.
    pub headers: Vec<(String, String)>,
    /// Upstream body, streamed through unchanged.
    pub body: Incoming,
    /// Upstream alias, used as the `host` metric label.
    pub host: String,
    /// Matched route pattern, used as the `http.route` metric label.
    pub route: String,
    /// Which side produced the response.
    pub error_source: ErrorSource,
}

/// Everything the pipeline needs after resolution, before execution.
struct ResolvedPlan {
    config: EffectiveConfig,
    route: Route,
    pattern: String,
    outbound_path: String,
    query: Option<String>,
    cors: CorsRequest,
    endpoint: Endpoint,
}

/// The data plane of the gear: turns a proxy request into an upstream call.
pub struct HttpProxyEngine {
    control_plane: Arc<ControlPlaneService>,
    chains: TenantChainResolver,
    transport: Arc<Transport>,
    plugins: PluginRegistry,
    limiters: LimiterRegistry,
    metrics: Arc<MetricsRegistry>,
    config: OagwConfig,
    cursor: AtomicU64,
}

impl std::fmt::Debug for HttpProxyEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither the plugin registry nor the limiter state implements `Debug`,
        // and neither holds a value worth rendering.
        f.debug_struct("HttpProxyEngine")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HttpProxyEngine {
    /// Builds the engine over an in-memory (or otherwise provisioned) control
    /// plane.
    #[must_use]
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        chains: TenantChainResolver,
        transport: Arc<Transport>,
        plugins: PluginRegistry,
        limiters: LimiterRegistry,
        metrics: Arc<MetricsRegistry>,
        config: OagwConfig,
    ) -> Self {
        Self {
            control_plane,
            chains,
            transport,
            plugins,
            limiters,
            metrics,
            config,
            cursor: AtomicU64::new(0),
        }
    }

    /// Metrics registry of this engine, for the admin endpoint.
    #[must_use]
    pub fn metrics(&self) -> &MetricsRegistry {
        &self.metrics
    }

    /// Executes one proxied request, recording metrics and the audit entry.
    ///
    /// # Errors
    ///
    /// Returns the matching entry of the DESIGN §3.3 error catalogue.
    pub async fn execute(&self, request: ProxyRequest) -> Result<ProxyOutcome, DomainError> {
        let started = std::time::Instant::now();
        self.metrics.begin_request();
        let outcome = self.proxy(&request).await;
        self.metrics.end_request();
        self.observe(&request, started, &outcome);
        outcome
    }

    // ------------------------------------------------------------------
    // Pipeline
    // ------------------------------------------------------------------

    /// Resolves and dispatches a request.
    async fn proxy(&self, request: &ProxyRequest) -> Result<ProxyOutcome, DomainError> {
        let plan = self.resolve(request).await?;
        let (context, quota) = self.prepare(request, &plan).await?;
        let outbound_request = outbound(request, &plan, &context);
        let response = self.transport.send(outbound_request).await?;
        self.finalise(&plan, quota, response).await
    }

    /// Resolves the outbound target of a WebSocket upgrade request (A1).
    ///
    /// Returns the `ws://`/`wss://` target the handler must connect to plus the
    /// extra outbound headers the resolved plugin chain produced. The frame
    /// pump itself lives in the transport handler, which owns the inbound
    /// axum upgrade.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`HttpProxyEngine::execute`].
    pub async fn websocket_target_for(
        &self,
        request: &ProxyRequest,
    ) -> Result<(String, Vec<(String, String)>), DomainError> {
        let plan = self.resolve(request).await?;
        let (context, _quota) = self.prepare(request, &plan).await?;
        let injected = context
            .outbound_headers
            .iter()
            .find(|(name, _)| name == OUTBOUND_QUERY_HEADER)
            .map(|(_, value)| value.as_str());
        let target = crate::infra::transport::websocket_target(
            &plan.endpoint,
            &plan.outbound_path,
            merged_query(&plan, injected).as_deref(),
        );
        let headers: Vec<(String, String)> = context
            .outbound_headers
            .iter()
            .filter(|(name, _)| {
                name != OUTBOUND_QUERY_HEADER && !name.eq_ignore_ascii_case("host")
            })
            .cloned()
            .collect();
        Ok((target, headers))
    }

    /// Steps 1–5: tenant chain, alias, merge, route, CORS and endpoint.
    async fn resolve(&self, request: &ProxyRequest) -> Result<ResolvedPlan, DomainError> {
        let chain = self.chains.chain(&request.security).await;
        let selected = self.selected_upstream(&chain, &request.alias).await?;
        // PRD §5.2: a disabled upstream must reject every proxy request with
        // 503 — never a silent fallback to an ancestor's upstream.
        if !selected.enabled {
            return Err(DomainError::LinkUnavailable {
                upstream_id: Some(selected.id),
                host: None,
            });
        }
        let mut config = self.effective_config(&chain, &selected).await?;
        let (route, suffix) = self.matched_route(&chain, &selected, request).await?;
        apply_route(&mut config, &route);

        let pairs = parse_query(request.query.as_deref());
        guard_route_match(&route, &borrowed(&pairs), &suffix)?;
        let cors = classify(&request.method, request.header("origin"), None);
        check_actual_request(config.cors.as_ref(), &cors, &request.method)?;
        let endpoint = self.select_endpoint(&config, request)?;

        let http = route.match_config.http.clone();
        Ok(ResolvedPlan {
            config,
            route,
            pattern: http_pattern(http.as_ref()),
            outbound_path: outbound_path(http.as_ref(), &suffix),
            query: allowed_query(request.query.as_deref(), &pairs),
            cors,
            endpoint,
        })
    }

    /// Steps 6–10: body validation, header transformation, plugins, quota.
    async fn prepare(
        &self,
        request: &ProxyRequest,
        plan: &ResolvedPlan,
    ) -> Result<(RequestContext, Option<RateLimitDecision>), DomainError> {
        validate_request_body(
            request.header("content-length"),
            request.header("transfer-encoding"),
            request.body.len() as u64,
            self.config.max_payload_bytes,
        )?;

        let mut context = Self::request_context(request, plan);
        self.run_auth(&mut context, plan).await?;
        self.run_guards(&context, plan).await?;
        self.run_transforms(&mut context, plan).await?;
        let quota = self.enforce_quota(request, plan)?;
        Ok((context, quota))
    }

    /// Response pipeline: guards, transforms, headers, CORS, quota labels.
    async fn finalise(
        &self,
        plan: &ResolvedPlan,
        quota: Option<RateLimitDecision>,
        response: UpstreamResponse,
    ) -> Result<ProxyOutcome, DomainError> {
        let mut context = ResponseContext {
            status: response.status,
            headers: response.headers.clone(),
            streaming: true,
            config: serde_json::Value::Null,
        };
        self.run_response_guards(&context, plan).await?;
        for plugin in plan
            .config
            .plugins
            .iter()
            .filter_map(|reference| self.plugins.transform(reference))
        {
            plugin.transform_response(&mut context).await?;
        }

        let mut headers =
            build_outbound_response_headers(&response.headers, response_rules(plan));
        apply_response_headers(plan.config.cors.as_ref(), &plan.cors, &mut headers);
        Self::append_quota_headers(plan, quota, &mut headers);
        set_header(&mut headers, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM);
        Ok(ProxyOutcome {
            status: response.status,
            headers,
            body: response.body,
            host: plan.config.upstream.normalized_alias(),
            route: plan.pattern.clone(),
            error_source: ErrorSource::Upstream,
        })
    }

    // ------------------------------------------------------------------
    // Resolution helpers
    // ------------------------------------------------------------------

    /// Walks the chain leaf → root and returns every upstream bound to `alias`.
    ///
    /// Review evidence (privilege boundary — cross-tenant isolation): only
    /// upstreams of tenants on the resolved chain are ever handed to
    /// [`resolve_alias`], so a caller cannot reach an unrelated tenant by
    /// guessing an alias.
    async fn alias_candidates(
        &self,
        chain: &[uuid::Uuid],
        alias_name: &str,
    ) -> Result<Vec<Upstream>, DomainError> {
        let mut candidates: Vec<Upstream> = Vec::new();
        for tenant in chain {
            for upstream in self.control_plane.list_upstreams(*tenant).await? {
                if upstream.normalized_alias() == alias_name {
                    candidates.push(upstream);
                }
            }
        }
        Ok(candidates)
    }

    /// Resolves the selected upstream for `alias` across the tenant chain.
    async fn selected_upstream(
        &self,
        chain: &[uuid::Uuid],
        alias_name: &str,
    ) -> Result<Upstream, DomainError> {
        let candidates = self.alias_candidates(chain, alias_name).await?;
        match resolve_alias(chain, &candidates, alias_name) {
            Some(resolved) => Ok(resolved.selected.clone()),
            None => Err(DomainError::LinkUnavailable {
                upstream_id: None,
                host: Some(alias_name.to_owned()),
            }),
        }
    }

    /// Folds the root → leaf chain of `selected` into its effective config.
    async fn effective_config(
        &self,
        chain: &[uuid::Uuid],
        selected: &Upstream,
    ) -> Result<EffectiveConfig, DomainError> {
        let mut chain_upstreams: Vec<Upstream> = Vec::new();
        for tenant in chain.iter().rev() {
            for upstream in self.control_plane.list_upstreams(*tenant).await? {
                if upstream.id == selected.id {
                    chain_upstreams.push(upstream);
                }
            }
        }
        let chain_refs: Vec<&Upstream> = chain_upstreams.iter().collect();
        Ok(merge_upstream_chain(&chain_refs))
    }

    /// Resolves the winning route of the selected upstream.
    ///
    /// Candidates are ordered leaf tenant first, so a descendant route wins a
    /// `(prefix, priority)` tie against its ancestor's route.
    async fn matched_route(
        &self,
        chain: &[uuid::Uuid],
        selected: &Upstream,
        request: &ProxyRequest,
    ) -> Result<(Route, String), DomainError> {
        let mut candidates: Vec<Route> = Vec::new();
        for tenant in chain {
            for route in self.control_plane.list_routes(*tenant).await? {
                if route.upstream_id == selected.id {
                    candidates.push(route);
                }
            }
        }
        match resolve_route(&candidates, &request.method, &request.path) {
            Some(matched) => Ok((matched.route.clone(), matched.suffix)),
            None => Err(DomainError::RouteNotFound {
                path: Some(request.path.clone()),
            }),
        }
    }

    /// Picks the upstream endpoint and records the routing metric.
    ///
    /// Review evidence (privilege boundary — egress): the endpoint produced
    /// here is the only place the outbound URL is built from, and
    /// [`ensure_allowed_scheme`] refuses a plaintext target unless the
    /// operator opted in.
    fn select_endpoint(
        &self,
        config: &EffectiveConfig,
        request: &ProxyRequest,
    ) -> Result<Endpoint, DomainError> {
        let cursor = self.cursor.fetch_add(1, Ordering::Relaxed);
        let target_host = request.header(TARGET_HOST_HEADER);
        let selected = select_endpoint(&config.upstream, target_host, cursor)?;
        self.metrics.record_endpoint_selection(
            &config.upstream.id.to_string(),
            &selected.endpoint.host_with_port(),
            selection_label(selected.method),
        );
        ensure_allowed_scheme(
            &config.upstream,
            selected.endpoint,
            self.control_plane.allows_http_upstream(),
        )?;
        Ok(selected.endpoint.clone())
    }

    // ------------------------------------------------------------------
    // Plugin chain
    // ------------------------------------------------------------------

    /// Builds the request context the plugins mutate.
    fn request_context(request: &ProxyRequest, plan: &ResolvedPlan) -> RequestContext {
        let headers = build_outbound_request_headers(
            &request.headers,
            &plan.endpoint.host_with_port(),
            request_rules(plan),
        );
        RequestContext {
            tenant_id: request.security.subject_tenant_id(),
            subject_id: request.security.subject_id(),
            upstream_id: Some(plan.config.upstream.id),
            route_id: Some(plan.route.id),
            path: request.path.clone(),
            method: request.method.clone(),
            headers: request.headers.clone(),
            outbound_headers: headers,
            config: serde_json::Value::Null,
        }
    }

    /// Runs the bound auth plugin (ADR 0002 step 1).
    async fn run_auth(
        &self,
        context: &mut RequestContext,
        plan: &ResolvedPlan,
    ) -> Result<(), DomainError> {
        let Some(auth) = plan.config.auth.as_ref() else {
            return Ok(());
        };
        let reference = auth
            .plugin_type
            .as_deref()
            .map(str::trim)
            .filter(|reference| !reference.is_empty())
            .ok_or_else(|| DomainError::PluginNotFound {
                plugin_id: String::new(),
                detail: "the upstream auth binding declares no plugin".to_owned(),
                upstream_id: Some(plan.config.upstream.id),
            })?;
        let plugin = self.plugins.auth(reference).ok_or_else(|| {
            DomainError::PluginNotFound {
                plugin_id: reference.to_owned(),
                detail: format!("auth plugin '{reference}' has no implementation in this process"),
                upstream_id: Some(plan.config.upstream.id),
            }
        })?;
        context.config = auth.config.clone().unwrap_or(serde_json::Value::Null);
        plugin.authenticate(context).await?;
        context.config = serde_json::Value::Null;
        Ok(())
    }

    /// Runs every guard plugin of the chain (ADR 0002 step 2).
    async fn run_guards(
        &self,
        context: &RequestContext,
        plan: &ResolvedPlan,
    ) -> Result<(), DomainError> {
        for plugin in plan
            .config
            .plugins
            .iter()
            .filter_map(|reference| self.plugins.guard(reference))
        {
            if let GuardDecision::Reject(error) = plugin.guard_request(context).await? {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Runs every request transform of the chain (ADR 0002 step 3).
    async fn run_transforms(
        &self,
        context: &mut RequestContext,
        plan: &ResolvedPlan,
    ) -> Result<(), DomainError> {
        for plugin in plan
            .config
            .plugins
            .iter()
            .filter_map(|reference| self.plugins.transform(reference))
        {
            plugin.transform_request(context).await?;
        }
        Ok(())
    }

    /// Runs the guard plugins against the upstream response.
    async fn run_response_guards(
        &self,
        context: &ResponseContext,
        plan: &ResolvedPlan,
    ) -> Result<(), DomainError> {
        for plugin in plan
            .config
            .plugins
            .iter()
            .filter_map(|reference| self.plugins.guard(reference))
        {
            if let GuardDecision::Reject(error) = plugin.guard_response(context).await? {
                return Err(error);
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Rate limiting (ADR 0003)
    // ------------------------------------------------------------------

    /// Applies the merged rate limit, if any, returning the decision.
    fn enforce_quota(
        &self,
        request: &ProxyRequest,
        plan: &ResolvedPlan,
    ) -> Result<Option<RateLimitDecision>, DomainError> {
        let Some(config) = plan.config.rate_limit.clone() else {
            return Ok(None);
        };
        let key = LimiterKey::new(&config, &quota_scope_key(request, plan, &config));
        let decision = self.limiters.check(&config, &key, now_millis());
        self.metrics.record_rate_usage(
            &plan.config.upstream.normalized_alias(),
            &plan.pattern,
            remaining_ratio(decision.remaining, config.effective_capacity()),
        );
        if decision.allowed {
            return Ok(Some(decision));
        }
        self.metrics
            .record_rate_limit_exceeded(&plan.config.upstream.normalized_alias(), &plan.pattern);
        Err(DomainError::RateLimitExceeded {
            retry_after_seconds: decision.retry_after_seconds,
            upstream_id: Some(plan.config.upstream.id),
            host: None,
            path: Some(plan.pattern.clone()),
        })
    }

    /// Appends the `X-RateLimit-*` response headers (ADR 0003).
    fn append_quota_headers(
        plan: &ResolvedPlan,
        quota: Option<RateLimitDecision>,
        headers: &mut Vec<(String, String)>,
    ) {
        let (Some(config), Some(decision)) = (plan.config.rate_limit.as_ref(), quota) else {
            return;
        };
        if !config.response_headers {
            return;
        }
        let parameters = RateLimitParameters::from_config(config);
        set_header(
            headers,
            RATE_LIMIT_REMAINING_HEADER,
            &decision.remaining.to_string(),
        );
        set_header(
            headers,
            RATE_LIMIT_LIMIT_HEADER,
            &parameters.capacity.to_string(),
        );
        set_header(
            headers,
            RATE_LIMIT_RESET_HEADER,
            &parameters.window_seconds.to_string(),
        );
    }

    // ------------------------------------------------------------------
    // Observability
    // ------------------------------------------------------------------

    /// Records the metrics and the audit entry of a completed request.
    fn observe(
        &self,
        request: &ProxyRequest,
        started: std::time::Instant,
        outcome: &Result<ProxyOutcome, DomainError>,
    ) {
        let method = normalize_method(&request.method);
        match outcome {
            Ok(success) => self.observe_success(request, started, method, success),
            Err(error) => self.observe_failure(request, started, method, error),
        }
    }

    /// Metrics and audit entry of a successfully proxied request.
    fn observe_success(
        &self,
        request: &ProxyRequest,
        started: std::time::Instant,
        method: &str,
        success: &ProxyOutcome,
    ) {
        let elapsed = started.elapsed().as_secs_f64();
        self.metrics
            .observe_duration(&success.host, &success.route, "total", elapsed);
        self.metrics
            .record_request(&success.host, method, &success.route, success.status);
        tracing::info!(
            event = "proxy.request",
            tenant_id = %request.security.subject_tenant_id(),
            principal_id = %request.security.subject_id(),
            host = %success.host,
            route = %success.route,
            method,
            status = success.status,
            duration_ms = elapsed * 1_000.0,
            request_size = request.body.len(),
            "proxied request served"
        );
    }

    /// Metrics and audit entry of a gateway-rejected request.
    fn observe_failure(
        &self,
        request: &ProxyRequest,
        started: std::time::Instant,
        method: &str,
        error: &DomainError,
    ) {
        // Review evidence (privilege boundary — observability): the metric and
        // the log line carry only the *normalised* alias, the matched pattern
        // and the error label — never a query string, a header value or a body.
        // The alias is truncated so a caller cannot grow the label cardinality
        // without bound.
        let host = metric_host(&request.alias);
        let elapsed = started.elapsed().as_secs_f64();
        let label = error_label(error);
        self.metrics.record_error(&host, UNMATCHED_ROUTE, &label);
        self.metrics
            .observe_duration(&host, UNMATCHED_ROUTE, "total", elapsed);
        tracing::warn!(
            event = "proxy.error",
            error_type = %label,
            status = error.status(),
            tenant_id = %request.security.subject_tenant_id(),
            principal_id = %request.security.subject_id(),
            host,
            route = UNMATCHED_ROUTE,
            method,
            duration_ms = elapsed * 1_000.0,
            "proxied request failed"
        );
    }
}

/// Outbound target of the request, splicing an auth plugin's query side-channel.
fn outbound(
    request: &ProxyRequest,
    plan: &ResolvedPlan,
    context: &RequestContext,
) -> UpstreamRequest {
    let injected = context
        .outbound_headers
        .iter()
        .find(|(name, _)| name == OUTBOUND_QUERY_HEADER)
        .map(|(_, value)| value.as_str());
    let url =
        request_target(&plan.endpoint, &plan.outbound_path, merged_query(plan, injected).as_deref());
    tracing::debug!(
        event = "proxy.outbound",
        url = %url,
        endpoint = %plan.endpoint.host_with_port(),
        scheme = %plan.endpoint.scheme.as_str(),
        header_count = context.outbound_headers.len(),
        "outbound request assembled"
    );
    UpstreamRequest {
        method: request.method.to_ascii_uppercase(),
        url,
        headers: context.outbound_headers.clone(),
        body: request.body.clone(),
    }
}

/// Combines the admitted query parameters with an injected side-channel query.
fn merged_query(plan: &ResolvedPlan, injected: Option<&str>) -> Option<String> {
    match (plan.query.as_deref(), injected) {
        (Some(base), Some(extra)) if !extra.is_empty() => Some(format!("{base}&{extra}")),
        (Some(base), None) => Some(base.to_owned()),
        (None, Some(extra)) if !extra.is_empty() => Some(extra.to_owned()),
        _ => None,
    }
}

/// Scope key the limiter counts on (ADR 0003 "Counting Scopes").
fn quota_scope_key(
    request: &ProxyRequest,
    plan: &ResolvedPlan,
    config: &RateLimitConfig,
) -> String {
    match config.scope {
        RateLimitScope::Global => "global".to_owned(),
        RateLimitScope::Tenant => request.security.subject_tenant_id().to_string(),
        RateLimitScope::User => request.security.subject_id().to_string(),
        RateLimitScope::Ip => request
            .header("x-forwarded-for")
            .unwrap_or_default()
            .to_owned(),
        RateLimitScope::Route => plan.route.id.to_string(),
    }
}

/// Share of the bucket still available, in the closed unit interval.
#[must_use]
pub fn remaining_ratio(remaining: u64, capacity: u64) -> f64 {
    if capacity == 0 {
        return 0.0;
    }
    let left = u32::try_from(remaining.min(u64::from(u32::MAX))).unwrap_or(0);
    let total = u32::try_from(capacity).unwrap_or(u32::MAX);
    f64::from(left) / f64::from(total)
}

/// Normalises and truncates an alias so it can be used as a metric label.
#[must_use]
pub fn metric_host(alias: &str) -> String {
    let normalised = alias::normalize_alias(alias);
    let mut truncated: String = normalised.chars().take(64).collect();
    if truncated.is_empty() {
        truncated.push_str("unknown");
    }
    truncated
}

/// Route label for the `http.route` metric: the pattern, never the raw path.
#[must_use]
pub fn http_pattern(matched: Option<&HttpMatch>) -> String {
    matched.map_or_else(|| "/".to_owned(), |http| http.path.clone())
}

/// Appends the suffix to the matched prefix, honouring `path_suffix_mode`.
#[must_use]
pub fn outbound_path(matched: Option<&HttpMatch>, suffix: &str) -> String {
    let Some(http) = matched else {
        return "/".to_owned();
    };
    let base = http.path.trim_end_matches('/');
    if http.path_suffix_mode == PathSuffixMode::Disabled || suffix.is_empty() {
        return if base.is_empty() {
            "/".to_owned()
        } else {
            base.to_owned()
        };
    }
    format!("{base}{suffix}")
}

/// Splits a raw query string into `name=value` pairs.
#[must_use]
pub fn parse_query(query: Option<&str>) -> Vec<(String, String)> {
    let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) else {
        return Vec::new();
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (name.to_owned(), value.to_owned()),
            None => (pair.to_owned(), String::new()),
        })
        .collect()
}

/// Re-renders the admitted query parameters as an outbound query string.
#[must_use]
pub fn allowed_query(raw: Option<&str>, pairs: &[(String, String)]) -> Option<String> {
    let _ = raw;
    if pairs.is_empty() {
        return None;
    }
    Some(
        pairs
            .iter()
            .map(|(name, value)| {
                if value.is_empty() {
                    name.clone()
                } else {
                    format!("{name}={value}")
                }
            })
            .collect::<Vec<_>>()
            .join("&"),
    )
}

/// Borrows pairs as the `(&str, &str)` slice the domain guard expects.
fn borrowed(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// Renders the metric label of a [`DomainError`] (`validation`, `route_not_found`, …).
#[must_use]
pub fn error_label(error: &DomainError) -> String {
    let gts = error.gts_type();
    let tail = gts.split('~').next_back().unwrap_or(gts);
    let trimmed = tail.strip_prefix("cf.oagw.").unwrap_or(tail);
    trimmed
        .strip_suffix(".v1")
        .unwrap_or(trimmed)
        .replace('.', "_")
}

/// Selection method as a metric label value.
#[must_use]
pub fn selection_label(method: SelectionMethod) -> &'static str {
    match method {
        SelectionMethod::ExplicitHeader => "explicit_header",
        SelectionMethod::RoundRobin => "round_robin",
        SelectionMethod::Default => "default",
    }
}

/// Request header rules of the plan, if any.
fn request_rules(plan: &ResolvedPlan) -> Option<&crate::domain::models::HeaderRules> {
    plan.config.headers.as_ref().and_then(|rules| rules.request.as_ref())
}

/// Response header rules of the plan, if any.
fn response_rules(plan: &ResolvedPlan) -> Option<&crate::domain::models::HeaderRules> {
    plan.config.headers.as_ref().and_then(|rules| rules.response.as_ref())
}

/// Sets (replacing) a header, keeping the insertion order.
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(slot) = headers
        .iter_mut()
        .find(|(name_, _)| name_.eq_ignore_ascii_case(name))
    {
        value.clone_into(&mut slot.1);
    } else {
        headers.push((name.to_owned(), value.to_owned()));
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
