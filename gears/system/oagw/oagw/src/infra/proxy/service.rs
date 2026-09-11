//! Data Plane: proxy request orchestration.
//!
//! One pass per request: resolve the alias across the tenant chain, match a
//! route, merge the effective configuration, run the plugin chain, forward,
//! and hand the response back still streaming. Every gateway-originated
//! answer carries `X-OAGW-Error-Source: gateway`; anything that came from the
//! upstream carries `upstream`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use hyper_util::rt::TokioIo;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::{
    ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, OagwError,
};
use crate::domain::gts_helpers as gts;
use crate::domain::model::{CorsConfig, Endpoint, PluginKind, RateStrategy, Route, Upstream};
use crate::domain::plugin::{ErrorContext, GuardDecision, RequestContext, ResponseContext};
use crate::domain::services::endpoint::{SelectionMethod, TARGET_HOST_HEADER, select_endpoint};
use crate::domain::services::management::ControlPlaneService;
use crate::domain::services::resolve::{self, EffectiveConfig, MatchedRoute, ResolvedUpstream};
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::proxy::circuit::{Admission, BreakerState, CircuitBreakerRegistry};
use crate::infra::proxy::connector::UpstreamConnector;
use crate::infra::proxy::{cors, headers};
use crate::infra::ratelimit::{RateLimitOutcome, RateLimiterRegistry, RateResource, ScopeKey};

/// Header set when a rate limit was exceeded but the configured strategy is
/// `degrade`, so a client can tell a degraded answer from a normal one.
pub const DEGRADED_HEADER: &str = "x-oagw-degraded";

/// Longest a `queue`-strategy request waits for rate limit capacity.
const MAX_QUEUE_WAIT: Duration = Duration::from_secs(5);

/// Everything the transport layer hands to the Data Plane.
pub struct ProxyRequest {
    /// Routing alias from the URL.
    pub alias: String,
    /// Raw path suffix from the URL, if any.
    pub path_suffix: Option<String>,
    /// Inbound method.
    pub method: Method,
    /// Inbound query parameters, in order.
    pub query: Vec<(String, String)>,
    /// Inbound headers.
    pub headers: HeaderMap,
    /// Inbound body, unread.
    pub body: Body,
    /// Client-side upgrade handle, when the request asked for one.
    pub on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    /// Client address, for `scope: ip` rate limits.
    pub client_ip: Option<String>,
    /// Caller identity.
    pub security_context: SecurityContext,
    /// Request path, used as the Problem Details `instance`.
    pub instance: String,
    /// Correlation id, when the edge assigned one.
    pub request_id: Option<String>,
}

/// Data Plane service.
pub struct DataPlaneService {
    control_plane: Arc<ControlPlaneService>,
    registries: Arc<PluginRegistries>,
    connector: Arc<UpstreamConnector>,
    limiter: Arc<RateLimiterRegistry>,
    breakers: Arc<CircuitBreakerRegistry>,
    metrics: Arc<OagwMetrics>,
    config: OagwConfig,
    round_robin: DashMap<Uuid, AtomicUsize>,
}

impl DataPlaneService {
    /// Wire the Data Plane to its collaborators.
    #[must_use]
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        registries: Arc<PluginRegistries>,
        connector: Arc<UpstreamConnector>,
        limiter: Arc<RateLimiterRegistry>,
        breakers: Arc<CircuitBreakerRegistry>,
        metrics: Arc<OagwMetrics>,
        config: OagwConfig,
    ) -> Self {
        Self {
            control_plane,
            registries,
            connector,
            limiter,
            breakers,
            metrics,
            config,
            round_robin: DashMap::new(),
        }
    }

    /// Rate limiter registry, so the Control Plane can drop counters when a
    /// resource is deleted.
    #[must_use]
    pub fn limiter(&self) -> &Arc<RateLimiterRegistry> {
        &self.limiter
    }

    /// Circuit breaker registry.
    #[must_use]
    pub fn breakers(&self) -> &Arc<CircuitBreakerRegistry> {
        &self.breakers
    }

    /// Execute a proxy request, returning the client-facing response.
    pub async fn execute(&self, request: ProxyRequest) -> Response {
        // A browser preflight carries no credentials, so it is answered here
        // without resolving an upstream (ADR-0004).
        if cors::is_preflight(&request.method, &request.headers) {
            let mut response = cors::PREFLIGHT_STATUS.into_response();
            for (name, value) in &cors::preflight_headers(&request.headers) {
                response.headers_mut().append(name.clone(), value.clone());
            }
            tag_gateway(&mut response);
            return response;
        }

        let alias = crate::domain::alias::normalize_alias(&request.alias);
        let instance = request.instance.clone();
        self.metrics.request_started(&alias);
        let started = Instant::now();
        let outcome = self.execute_inner(request, &alias).await;
        let response = match outcome {
            Ok(response) => response,
            Err(err) => {
                self.metrics.record_error(&alias, &instance, err.error_type);
                problem_response(&err, Some(&instance))
            }
        };
        self.metrics
            .record_duration(&alias, &instance, "total", started.elapsed().as_secs_f64());
        self.metrics.request_finished(&alias);
        response
    }

    async fn execute_inner(
        &self,
        request: ProxyRequest,
        alias: &str,
    ) -> Result<Response, OagwError> {
        let tenant_id = request.security_context.subject_tenant_id();
        if tenant_id.is_nil() {
            return Err(OagwError::forbidden(
                "the request carries no tenant; proxying is tenant-scoped",
            ));
        }
        headers::validate_body_framing(&request.headers, self.config.max_body_bytes)?;

        let resolved = self
            .resolve_alias(&request.security_context, tenant_id, alias)
            .await?;
        let matched = self
            .match_route(
                &request.method,
                request.path_suffix.as_deref(),
                &resolved,
                alias,
            )
            .await?;
        let effective = resolve::effective_config(&resolved, Some(&matched.route));
        if !effective.enabled {
            return Err(
                OagwError::upstream_disabled(format!("upstream '{alias}' is disabled"))
                    .with("upstream_id", upstream_id(&resolved.selected))
                    .with("alias", alias.to_owned()),
            );
        }

        let origin = cors::enforce_actual_request(
            effective.cors.as_ref(),
            &request.method,
            &request.headers,
        )?;

        let http_match = matched
            .route
            .http()
            .ok_or_else(|| OagwError::route_not_found("matched route has no HTTP match rules"))?
            .clone();
        let query = resolve::filter_query(&http_match, &request.query)?;

        let target_host = request
            .headers
            .get(TARGET_HOST_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let (endpoint, selection) = select_endpoint(
            &resolved.selected,
            target_host.as_deref(),
            self.next_index(&resolved.selected),
        )?;
        self.metrics.record_endpoint_selection(
            &upstream_id(&resolved.selected),
            &endpoint.host,
            selection.as_str(),
        );

        let breaker_key = format!(
            "{}|{}:{}",
            resolved.selected.id, endpoint.host, endpoint.port
        );
        if let Admission::Refuse { retry_after } = self.breakers.admit(&breaker_key) {
            return Err(OagwError::circuit_breaker_open(format!(
                "circuit breaker for '{}' is open",
                endpoint.host
            ))
            .with("host", endpoint.host.clone())
            .with_retry_after(retry_after));
        }

        let scope_key = ScopeKey {
            tenant_id,
            subject_id: request.security_context.subject_id(),
            client_ip: request.client_ip.clone(),
            route_id: Some(matched.route.id),
        };
        let rate_headers = self
            .enforce_rate_limit(&scope_key, &resolved, &matched.route, &effective, alias)
            .await?;

        self.forward(
            request,
            &resolved,
            &matched,
            &effective,
            &endpoint,
            &query,
            origin,
            rate_headers,
            &breaker_key,
            selection,
        )
        .await
    }

    /// Walk the tenant chain descendant → root and collect same-alias
    /// upstreams, closest first.
    async fn resolve_alias(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<ResolvedUpstream, OagwError> {
        let chain = self
            .control_plane
            .tenant_directory()
            .chain(ctx, tenant_id)
            .await;
        let mut candidates = Vec::new();
        for tenant in &chain {
            if let Some(upstream) = self
                .control_plane
                .upstream_repo()
                .find_by_alias(*tenant, alias)
                .await
            {
                candidates.push(upstream);
            }
        }
        ResolvedUpstream::from_chain(candidates).ok_or_else(|| {
            OagwError::route_not_found(format!(
                "no upstream is registered for alias '{alias}' in this tenant hierarchy"
            ))
            .with("alias", alias.to_owned())
        })
    }

    async fn match_route(
        &self,
        method: &Method,
        path_suffix: Option<&str>,
        resolved: &ResolvedUpstream,
        alias: &str,
    ) -> Result<MatchedRoute, OagwError> {
        let suffix = resolve::normalize_suffix(path_suffix);
        let mut levels: Vec<Vec<Route>> = Vec::new();
        for upstream in std::iter::once(&resolved.selected).chain(resolved.ancestors.iter()) {
            levels.push(
                self.control_plane
                    .route_repo()
                    .list_by_upstream(upstream.id)
                    .await,
            );
        }
        let (route, remaining) = resolve::match_route_in_chain(&levels, method.as_str(), &suffix)
            .ok_or_else(|| {
            OagwError::route_not_found(format!(
                "no route on upstream '{alias}' matches {method} {suffix}"
            ))
            .with("alias", alias.to_owned())
            .with("upstream_id", upstream_id(&resolved.selected))
            .with("path", suffix.clone())
        })?;
        let http = route
            .http()
            .ok_or_else(|| OagwError::route_not_found("matched route has no HTTP match rules"))?;
        let outbound_path = resolve::apply_path_suffix(http, &remaining)?;
        Ok(MatchedRoute {
            route,
            outbound_path,
        })
    }

    fn next_index(&self, upstream: &Upstream) -> usize {
        self.round_robin
            .entry(upstream.id)
            .or_insert_with(|| AtomicUsize::new(0))
            .fetch_add(1, Ordering::Relaxed)
    }

    /// Apply the effective rate limit, returning the `X-RateLimit-*` headers
    /// to attach to the response.
    async fn enforce_rate_limit(
        &self,
        scope_key: &ScopeKey,
        resolved: &ResolvedUpstream,
        route: &Route,
        effective: &EffectiveConfig,
        alias: &str,
    ) -> Result<Vec<(HeaderName, HeaderValue)>, OagwError> {
        let Some(config) = &effective.rate_limit else {
            return Ok(Vec::new());
        };
        // Counters live on the resource the limit is most specific to, so a
        // route-level limit does not consume the upstream-level budget.
        let resource = if route.spec.rate_limit.is_some() {
            RateResource::Route(route.id)
        } else {
            RateResource::Upstream(resolved.selected.id)
        };
        let mut outcome = self.limiter.check(resource, config, scope_key);
        if matches!(outcome, RateLimitOutcome::Rejected { .. })
            && config.strategy == RateStrategy::Queue
        {
            let wait = self.limiter.queue_delay(resource, config, scope_key);
            let wait = Duration::from_secs_f64(wait.clamp(0.0, MAX_QUEUE_WAIT.as_secs_f64()));
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
                outcome = self.limiter.check(resource, config, scope_key);
            }
        }

        match outcome {
            RateLimitOutcome::Allowed {
                limit,
                remaining,
                reset_after,
            } => {
                if config.response_headers {
                    #[allow(clippy::cast_precision_loss)]
                    let ratio = 1.0 - (remaining as f64 / limit.max(1) as f64);
                    self.metrics
                        .record_rate_limit_usage(alias, &route_pattern(route), ratio);
                    return Ok(rate_limit_headers(limit, Some(remaining), reset_after));
                }
                Ok(Vec::new())
            }
            RateLimitOutcome::Degraded { limit, retry_after } => {
                self.metrics
                    .record_rate_limit_exceeded(alias, &route_pattern(route));
                tracing::warn!(
                    target: "oagw.ratelimit",
                    host = alias,
                    path = %route_pattern(route),
                    "rate limit exceeded; serving in degraded mode"
                );
                let mut out = if config.response_headers {
                    rate_limit_headers(limit, Some(0), retry_after)
                } else {
                    Vec::new()
                };
                if let Ok(name) = DEGRADED_HEADER.parse::<HeaderName>() {
                    out.push((name, HeaderValue::from_static("true")));
                }
                Ok(out)
            }
            RateLimitOutcome::Rejected { limit, retry_after } => {
                self.metrics
                    .record_rate_limit_exceeded(alias, &route_pattern(route));
                tracing::warn!(
                    target: "oagw.ratelimit",
                    host = alias,
                    path = %route_pattern(route),
                    retry_after,
                    "rate limit exceeded; refusing the request"
                );
                let mut err = OagwError::rate_limit_exceeded(format!(
                    "Rate limit exceeded for upstream {alias}"
                ))
                .with("host", alias.to_owned())
                .with("upstream_id", upstream_id(&resolved.selected))
                .with("limit", limit)
                .with_retry_after(retry_after);
                if config.response_headers {
                    for (name, value) in rate_limit_headers(limit, Some(0), retry_after) {
                        err = err.with_header(name.as_str(), value.to_str().unwrap_or("0"));
                    }
                }
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        request: ProxyRequest,
        resolved: &ResolvedUpstream,
        matched: &MatchedRoute,
        effective: &EffectiveConfig,
        endpoint: &Endpoint,
        query: &[(String, String)],
        origin: Option<String>,
        rate_headers: Vec<(HeaderName, HeaderValue)>,
        breaker_key: &str,
        selection: SelectionMethod,
    ) -> Result<Response, OagwError> {
        let ProxyRequest {
            method,
            headers: inbound_headers,
            body,
            on_upgrade: client_upgrade,
            security_context,
            request_id,
            ..
        } = request;

        let alias = resolved.selected.alias().to_owned();
        let is_upgrade = headers::is_upgrade_request(&inbound_headers);
        let request_rules = effective.headers.as_ref().and_then(|h| h.request.as_ref());
        let outbound_headers =
            headers::build_request_headers(&inbound_headers, request_rules, is_upgrade)?;

        let mut ctx = RequestContext {
            method: method.clone(),
            path: matched.outbound_path.clone(),
            query: query.to_vec(),
            headers: outbound_headers,
            config: serde_json::Map::new(),
            security_context: security_context.clone(),
            alias: alias.clone(),
            upstream_id: resolved.selected.id,
            route_id: Some(matched.route.id),
        };

        // Auth → Guards → Transform(on_request), upstream bindings before
        // route bindings (ADR-0002).
        let plugin_started = Instant::now();
        self.run_auth(effective, &mut ctx).await?;
        if let Some(rejection) = self.run_guards_request(effective, &mut ctx).await? {
            return Ok(rejection);
        }
        self.run_transform_request(effective, &mut ctx).await?;
        self.metrics.record_duration(
            &alias,
            &route_pattern(&matched.route),
            "plugins",
            plugin_started.elapsed().as_secs_f64(),
        );

        // Absolute form for logs and metrics; the wire request carries
        // origin-form, which is what RFC 9112 §3.2.1 prescribes for a direct
        // (non-proxy) request and what upstreams echo back as `path`.
        let absolute = build_uri(endpoint, &ctx.path, &ctx.query)?;
        let mut builder = http::Request::builder()
            .method(ctx.method.clone())
            .uri(origin_form(&ctx.path, &ctx.query)?);
        tracing::debug!(
            target: "oagw.proxy",
            method = %ctx.method,
            target = %absolute,
            "forwarding to upstream"
        );
        {
            let target = builder
                .headers_mut()
                .ok_or_else(|| OagwError::internal("could not build the outbound request"))?;
            for (name, value) in &ctx.headers {
                target.append(name.clone(), value.clone());
            }
            // The inbound Host is replaced by the upstream authority.
            if let Ok(host) = HeaderValue::from_str(&authority(endpoint)) {
                target.insert(http::header::HOST, host);
            }
        }
        let outbound = builder
            .body(body)
            .map_err(|err| OagwError::internal(format!("outbound request is invalid: {err}")))?;

        let upstream_started = Instant::now();
        let sent = self.connector.send(endpoint, outbound).await;
        self.metrics.record_duration(
            &alias,
            &route_pattern(&matched.route),
            "upstream",
            upstream_started.elapsed().as_secs_f64(),
        );

        let sent = match sent {
            Ok(sent) => {
                if self.breakers.record_success(breaker_key) != BreakerState::Closed {
                    self.metrics.record_circuit_breaker(
                        &alias,
                        "open",
                        "closed",
                        BreakerState::Closed.as_gauge(),
                    );
                }
                self.metrics
                    .record_upstream_available(&alias, &endpoint.host, true);
                sent
            }
            Err(err) => {
                let state = self.breakers.record_failure(breaker_key);
                if state == BreakerState::Open {
                    self.metrics
                        .record_circuit_breaker(&alias, "closed", "open", state.as_gauge());
                }
                self.metrics
                    .record_upstream_available(&alias, &endpoint.host, false);
                self.run_transform_error(effective, &err).await;
                return Err(err
                    .with("host", endpoint.host.clone())
                    .with("upstream_id", upstream_id(&resolved.selected))
                    .with("path", matched.outbound_path.clone()));
            }
        };

        let status = sent.parts.status;
        let mut response_headers = sent.parts.headers.clone();
        let upgraded = sent.on_upgrade.is_some() && status == StatusCode::SWITCHING_PROTOCOLS;
        headers::strip_hop_by_hop_response(&mut response_headers, upgraded);
        headers::apply_response_rules(
            &mut response_headers,
            effective.headers.as_ref().and_then(|h| h.response.as_ref()),
        )?;

        let mut response_ctx = ResponseContext {
            status,
            headers: response_headers,
            config: serde_json::Map::new(),
            request_id: request_id.clone(),
        };
        if let Some(rejection) = self
            .run_guards_response(effective, &mut response_ctx)
            .await?
        {
            return Ok(rejection);
        }
        self.run_transform_response(effective, &mut response_ctx)
            .await?;
        let mut response_headers = response_ctx.headers;

        if let (Some(cors_config), Some(origin)) = (effective.cors.as_ref(), origin.as_deref()) {
            cors::apply_response_headers(&mut response_headers, cors_config, origin);
        }
        for (name, value) in rate_headers {
            response_headers.insert(name, value);
        }

        self.metrics.record_request(
            &alias,
            method.as_str(),
            &route_pattern(&matched.route),
            status.as_u16(),
        );
        log_proxy_request(
            &AuditFields {
                request_id: request_id.as_deref(),
                security_context: &security_context,
                method: &method,
            },
            &alias,
            &matched.outbound_path,
            status.as_u16(),
            selection,
        );

        if upgraded {
            return Ok(self.finish_upgrade(
                client_upgrade,
                sent.on_upgrade,
                status,
                response_headers,
            ));
        }

        let mut response = Response::new(Body::new(sent.body));
        *response.status_mut() = status;
        *response.headers_mut() = response_headers;
        tag_upstream(&mut response);
        Ok(response)
    }

    /// Complete a `101` by joining the two upgraded transports.
    fn finish_upgrade(
        &self,
        client: Option<hyper::upgrade::OnUpgrade>,
        upstream: Option<hyper::upgrade::OnUpgrade>,
        status: StatusCode,
        response_headers: HeaderMap,
    ) -> Response {
        match (client, upstream) {
            (Some(client), Some(upstream)) => {
                tokio::spawn(async move {
                    match tokio::try_join!(client, upstream) {
                        Ok((client_io, upstream_io)) => {
                            let mut client_io = TokioIo::new(client_io);
                            let mut upstream_io = TokioIo::new(upstream_io);
                            match tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io)
                                .await
                            {
                                Ok((to_upstream, to_client)) => tracing::debug!(
                                    target: "oagw.proxy",
                                    to_upstream,
                                    to_client,
                                    "upgraded stream closed"
                                ),
                                Err(err) => tracing::debug!(
                                    target: "oagw.proxy",
                                    error = %err,
                                    "upgraded stream aborted"
                                ),
                            }
                        }
                        Err(err) => tracing::warn!(
                            target: "oagw.proxy",
                            error = %err,
                            "protocol upgrade could not be completed"
                        ),
                    }
                });
                let mut response = Response::new(Body::empty());
                *response.status_mut() = status;
                *response.headers_mut() = response_headers;
                tag_upstream(&mut response);
                response
            }
            _ => {
                // The upstream switched protocols but the client never asked
                // to: there is nothing to bridge the raw stream onto.
                let err = OagwError::protocol(
                    "upstream switched protocols but the client did not request an upgrade",
                );
                problem_response(&err, None)
            }
        }
    }

    // -- plugin chain execution ---------------------------------------------

    async fn run_auth(
        &self,
        effective: &EffectiveConfig,
        ctx: &mut RequestContext,
    ) -> Result<(), OagwError> {
        let Some(auth) = &effective.auth else {
            return Ok(());
        };
        let Some(plugin_ref) = &auth.plugin_type else {
            return Ok(());
        };
        let plugin = match self.registries.auth.get(plugin_ref) {
            Some(plugin) => plugin,
            None => {
                if let Some(uuid) = gts::plugin_ref_uuid(plugin_ref) {
                    self.control_plane.plugin_repo().touch(uuid).await;
                    return Err(OagwError::plugin_not_found(format!(
                        "custom auth plugin '{plugin_ref}' cannot be executed: this build ships \
                         no sandboxed script engine"
                    )));
                }
                return Err(OagwError::plugin_not_found(format!(
                    "unknown auth plugin: {plugin_ref}"
                )));
            }
        };
        ctx.config = auth.config.clone();
        let result = plugin.authenticate(ctx).await;
        ctx.config = serde_json::Map::new();
        result.map_err(OagwError::from)
    }

    async fn run_guards_request(
        &self,
        effective: &EffectiveConfig,
        ctx: &mut RequestContext,
    ) -> Result<Option<Response>, OagwError> {
        for binding in &effective.plugins {
            if PluginKind::from_plugin_ref(&binding.plugin_ref) != Some(PluginKind::Guard) {
                continue;
            }
            let plugin = self.resolve_guard(&binding.plugin_ref).await?;
            ctx.config = binding.config.clone();
            let decision = plugin.guard_request(ctx).await;
            ctx.config = serde_json::Map::new();
            if let GuardDecision::Reject {
                status,
                error_code,
                message,
            } = decision.map_err(OagwError::from)?
            {
                let err = guard_rejection(status, &error_code, &message, plugin.plugin_type());
                return Ok(Some(problem_response(&err, None)));
            }
        }
        Ok(None)
    }

    async fn run_guards_response(
        &self,
        effective: &EffectiveConfig,
        ctx: &mut ResponseContext,
    ) -> Result<Option<Response>, OagwError> {
        for binding in &effective.plugins {
            if PluginKind::from_plugin_ref(&binding.plugin_ref) != Some(PluginKind::Guard) {
                continue;
            }
            let plugin = self.resolve_guard(&binding.plugin_ref).await?;
            ctx.config = binding.config.clone();
            let decision = plugin.guard_response(ctx).await;
            ctx.config = serde_json::Map::new();
            if let GuardDecision::Reject {
                status,
                error_code,
                message,
            } = decision.map_err(OagwError::from)?
            {
                let err = guard_rejection(status, &error_code, &message, plugin.plugin_type());
                return Ok(Some(problem_response(&err, None)));
            }
        }
        Ok(None)
    }

    async fn resolve_guard(
        &self,
        plugin_ref: &str,
    ) -> Result<Arc<dyn crate::domain::plugin::GuardPlugin>, OagwError> {
        if let Some(plugin) = self.registries.guard.get(plugin_ref) {
            return Ok(plugin);
        }
        if let Some(uuid) = gts::plugin_ref_uuid(plugin_ref) {
            self.control_plane.plugin_repo().touch(uuid).await;
            return Err(OagwError::plugin_not_found(format!(
                "custom guard plugin '{plugin_ref}' cannot be executed: this build ships no \
                 sandboxed script engine"
            )));
        }
        Err(OagwError::plugin_not_found(format!(
            "unknown guard plugin: {plugin_ref}"
        )))
    }

    async fn resolve_transform(
        &self,
        plugin_ref: &str,
    ) -> Result<Arc<dyn crate::domain::plugin::TransformPlugin>, OagwError> {
        if let Some(plugin) = self.registries.transform.get(plugin_ref) {
            return Ok(plugin);
        }
        if let Some(uuid) = gts::plugin_ref_uuid(plugin_ref) {
            self.control_plane.plugin_repo().touch(uuid).await;
            return Err(OagwError::plugin_not_found(format!(
                "custom transform plugin '{plugin_ref}' cannot be executed: this build ships no \
                 sandboxed script engine"
            )));
        }
        Err(OagwError::plugin_not_found(format!(
            "unknown transform plugin: {plugin_ref}"
        )))
    }

    async fn run_transform_request(
        &self,
        effective: &EffectiveConfig,
        ctx: &mut RequestContext,
    ) -> Result<(), OagwError> {
        for binding in &effective.plugins {
            if PluginKind::from_plugin_ref(&binding.plugin_ref) != Some(PluginKind::Transform) {
                continue;
            }
            let plugin = self.resolve_transform(&binding.plugin_ref).await?;
            ctx.config = binding.config.clone();
            let result = plugin.transform_request(ctx).await;
            ctx.config = serde_json::Map::new();
            result.map_err(OagwError::from)?;
        }
        Ok(())
    }

    async fn run_transform_response(
        &self,
        effective: &EffectiveConfig,
        ctx: &mut ResponseContext,
    ) -> Result<(), OagwError> {
        for binding in &effective.plugins {
            if PluginKind::from_plugin_ref(&binding.plugin_ref) != Some(PluginKind::Transform) {
                continue;
            }
            let plugin = self.resolve_transform(&binding.plugin_ref).await?;
            ctx.config = binding.config.clone();
            let result = plugin.transform_response(ctx).await;
            ctx.config = serde_json::Map::new();
            result.map_err(OagwError::from)?;
        }
        Ok(())
    }

    /// Best-effort `on_error` pass; a transform failure here must not mask the
    /// original upstream failure.
    async fn run_transform_error(&self, effective: &EffectiveConfig, err: &OagwError) {
        for binding in &effective.plugins {
            if PluginKind::from_plugin_ref(&binding.plugin_ref) != Some(PluginKind::Transform) {
                continue;
            }
            let Ok(plugin) = self.resolve_transform(&binding.plugin_ref).await else {
                continue;
            };
            let mut ctx = ErrorContext {
                error_type: err.error_type.to_owned(),
                status: StatusCode::from_u16(err.status).unwrap_or(StatusCode::BAD_GATEWAY),
                headers: HeaderMap::new(),
                config: binding.config.clone(),
            };
            if let Err(plugin_err) = plugin.transform_error(&mut ctx).await {
                tracing::debug!(
                    target: "oagw.proxy",
                    plugin = %binding.plugin_ref,
                    error = %plugin_err,
                    "on_error transform failed"
                );
            }
        }
    }
}

// -- free helpers ----------------------------------------------------------

fn upstream_id(upstream: &Upstream) -> String {
    gts::anonymous_id(gts::UPSTREAM_TYPE, upstream.id)
}

/// Metric / log label for a route: the configured pattern, never the raw
/// request path (cardinality).
fn route_pattern(route: &Route) -> String {
    route
        .http()
        .map_or_else(|| "-".to_owned(), |http| http.path.clone())
}

fn authority(endpoint: &Endpoint) -> String {
    if endpoint.port == endpoint.standard_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

/// Wire scheme for an endpoint: WebSocket and WebTransport ride the matching
/// HTTP scheme on the wire.
fn wire_scheme(endpoint: &Endpoint) -> &'static str {
    match endpoint.scheme.as_str() {
        "http" | "ws" => "http",
        _ => "https",
    }
}

/// Build the absolute outbound URI.
///
/// # Errors
///
/// Returns `400` when the composed URI is not valid.
pub fn build_uri(
    endpoint: &Endpoint,
    path: &str,
    query: &[(String, String)],
) -> Result<http::Uri, OagwError> {
    let mut url = format!(
        "{}://{}{}",
        wire_scheme(endpoint),
        authority(endpoint),
        if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        }
    );
    if !query.is_empty() {
        let encoded: String = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish();
        url.push('?');
        url.push_str(&encoded);
    }
    url.parse::<http::Uri>()
        .map_err(|err| OagwError::validation(format!("could not build the upstream URI: {err}")))
}

/// Build the origin-form request target: `/path[?query]`.
///
/// # Errors
///
/// Returns `400` when the composed target is not a valid URI.
pub fn origin_form(path: &str, query: &[(String, String)]) -> Result<http::Uri, OagwError> {
    let mut target = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    if !query.is_empty() {
        let encoded: String = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish();
        target.push('?');
        target.push_str(&encoded);
    }
    target.parse::<http::Uri>().map_err(|err| {
        OagwError::validation(format!(
            "could not build the upstream request target: {err}"
        ))
    })
}

fn rate_limit_headers(
    limit: u64,
    remaining: Option<u64>,
    reset_after: u64,
) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::new();
    let mut push = |name: &'static str, value: String| {
        if let (Ok(name), Ok(value)) = (name.parse::<HeaderName>(), HeaderValue::from_str(&value)) {
            out.push((name, value));
        }
    };
    push("x-ratelimit-limit", limit.to_string());
    if let Some(remaining) = remaining {
        push("x-ratelimit-remaining", remaining.to_string());
    }
    push("x-ratelimit-reset", reset_after.to_string());
    out
}

fn guard_rejection(
    status: StatusCode,
    error_code: &str,
    message: &str,
    plugin_type: &str,
) -> OagwError {
    let mut err = if status.as_u16() >= 500 {
        OagwError::protocol(message.to_owned())
    } else {
        OagwError::validation(message.to_owned())
    };
    err.status = status.as_u16();
    err.with("error_code", error_code.to_owned())
        .with("plugin", plugin_type.to_owned())
}

/// Render an [`OagwError`] as an RFC 9457 Problem Details response.
#[must_use]
pub fn problem_response(err: &OagwError, instance: Option<&str>) -> Response {
    let body = err.to_problem(instance).to_string();
    let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    if let Some(seconds) = err.retry_after_seconds
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, value);
    }
    for (name, value) in &err.headers {
        if let (Ok(name), Ok(value)) = (name.parse::<HeaderName>(), HeaderValue::from_str(value)) {
            response.headers_mut().insert(name, value);
        }
    }
    tag_gateway(&mut response);
    response
}

fn tag_gateway(response: &mut Response) {
    if let Ok(name) = ERROR_SOURCE_HEADER.parse::<HeaderName>() {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(ERROR_SOURCE_GATEWAY));
    }
}

fn tag_upstream(response: &mut Response) {
    if let Ok(name) = ERROR_SOURCE_HEADER.parse::<HeaderName>() {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(ERROR_SOURCE_UPSTREAM));
    }
}

/// The subset of a proxy request that appears in the audit log.
struct AuditFields<'a> {
    request_id: Option<&'a str>,
    security_context: &'a SecurityContext,
    method: &'a Method,
}

/// Structured audit line for a proxied request.
///
/// No bodies, no query strings, no headers — only the fields DESIGN §4.3
/// enumerates.
fn log_proxy_request(
    fields: &AuditFields<'_>,
    host: &str,
    path: &str,
    status: u16,
    selection: SelectionMethod,
) {
    tracing::info!(
        target: "oagw.audit",
        event = "proxy_request",
        request_id = fields.request_id.unwrap_or("-"),
        tenant_id = %fields.security_context.subject_tenant_id(),
        principal_id = %fields.security_context.subject_id(),
        host,
        path,
        method = %fields.method,
        status,
        endpoint_selection = selection.as_str(),
        "proxied request completed"
    );
}

/// Effective CORS policy for a resolved request, exposed for tests.
#[must_use]
pub fn effective_cors(effective: &EffectiveConfig) -> Option<&CorsConfig> {
    effective.cors.as_ref()
}

#[cfg(test)]
mod tests {
    use super::{
        authority, build_uri, origin_form, problem_response, rate_limit_headers, wire_scheme,
    };
    use crate::domain::error::{ERROR_SOURCE_HEADER, OagwError};
    use crate::domain::model::Endpoint;

    fn endpoint(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn wire_schemes_map_websocket_onto_http() {
        assert_eq!(wire_scheme(&endpoint("ws", "h", 80)), "http");
        assert_eq!(wire_scheme(&endpoint("http", "h", 80)), "http");
        assert_eq!(wire_scheme(&endpoint("wss", "h", 443)), "https");
        assert_eq!(wire_scheme(&endpoint("https", "h", 443)), "https");
        assert_eq!(wire_scheme(&endpoint("wt", "h", 443)), "https");
    }

    #[test]
    fn authority_omits_the_standard_port() {
        assert_eq!(
            authority(&endpoint("https", "api.openai.com", 443)),
            "api.openai.com"
        );
        assert_eq!(authority(&endpoint("http", "mock.local", 80)), "mock.local");
        assert_eq!(
            authority(&endpoint("http", "mock.local", 8080)),
            "mock.local:8080"
        );
    }

    #[test]
    fn uri_composition() {
        let uri = build_uri(
            &endpoint("https", "api.openai.com", 443),
            "/v1/chat/completions",
            &[],
        )
        .expect("built");
        assert_eq!(
            uri.to_string(),
            "https://api.openai.com/v1/chat/completions"
        );

        let uri = build_uri(
            &endpoint("http", "mock.local", 8080),
            "/v1/models",
            &[("limit".to_owned(), "5".to_owned())],
        )
        .expect("built");
        assert_eq!(uri.to_string(), "http://mock.local:8080/v1/models?limit=5");
    }

    #[test]
    fn uri_composition_percent_encodes_query_values() {
        let uri = build_uri(
            &endpoint("https", "api.openai.com", 443),
            "/search",
            &[("q".to_owned(), "a b&c".to_owned())],
        )
        .expect("built");
        assert!(uri.to_string().ends_with("/search?q=a+b%26c"), "{uri}");
    }

    #[test]
    fn a_missing_leading_slash_is_repaired() {
        let uri = build_uri(&endpoint("https", "api.openai.com", 443), "v1", &[]).expect("built");
        assert_eq!(uri.to_string(), "https://api.openai.com/v1");
    }

    #[test]
    fn the_wire_target_is_origin_form() {
        let uri =
            origin_form("/v1/models", &[("limit".to_owned(), "5".to_owned())]).expect("built");
        assert_eq!(uri.to_string(), "/v1/models?limit=5");
        assert_eq!(origin_form("v1", &[]).expect("built").to_string(), "/v1");
        assert_eq!(origin_form("/", &[]).expect("built").to_string(), "/");
    }

    #[test]
    fn rate_limit_header_set() {
        let out = rate_limit_headers(100, Some(42), 30);
        let names: Vec<String> = out.iter().map(|(n, _)| n.as_str().to_owned()).collect();
        assert_eq!(
            names,
            vec![
                "x-ratelimit-limit",
                "x-ratelimit-remaining",
                "x-ratelimit-reset"
            ]
        );
        assert_eq!(out[1].1, "42");
    }

    #[test]
    fn problem_responses_are_tagged_as_gateway_errors() {
        let err = OagwError::rate_limit_exceeded("too fast").with_retry_after(15);
        let response = problem_response(&err, Some("/oagw/v1/proxy/a/b"));
        assert_eq!(response.status(), 429);
        assert_eq!(
            response.headers()[http::header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert_eq!(response.headers()[ERROR_SOURCE_HEADER], "gateway");
        assert_eq!(response.headers()[http::header::RETRY_AFTER], "15");
    }
}
