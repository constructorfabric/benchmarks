//! `DataPlaneService` — proxy orchestration.
//!
//! One function owns the whole request path so the order the ADRs fix is
//! visible in one place (`cpt-cf-oagw-adr-plugin-system`,
//! `cpt-cf-oagw-adr-state-management`):
//!
//! ```text
//! preflight? → resolve config → CORS → validate → select endpoint
//!   → auth plugin → rate limit → guards → transform(request)
//!   → upstream (HTTP | SSE stream | WebSocket splice)
//!   → guard(response) → transform(response) → client
//! ```
//!
//! Every response carries `X-OAGW-Error-Source`
//! (`cpt-cf-oagw-adr-error-source-distinction`): `gateway` when OAGW produced
//! it, `upstream` when it came back from the external service.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Instant;

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use dashmap::DashMap;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::dto::ResolvedTarget;
use crate::domain::error::{
    ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, ErrorKind, OagwError, OagwResult,
    TARGET_HOST_HEADER,
};
use crate::domain::gts_helpers::{PluginKind, UPSTREAM_TYPE, anonymous_id, plugin_ref_uuid};
use crate::domain::model::{PathSuffixMode, PluginBinding};
use crate::domain::plugin::{
    AuthContext, ErrorContext, GuardDecision, RequestContext, ResponseContext,
};
use crate::domain::services::management::ControlPlane;
use crate::domain::services::resolve::{normalize_request_path, path_remainder};
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::proxy::{connect, cors, headers as header_rules, upstream_http, websocket};
use crate::infra::rate_limit::{RateLimiterRegistry, counter_key, rejects_on_limit};

/// Everything the transport layer hands the Data Plane for one request.
pub struct ProxyRequest {
    /// Caller identity.
    pub security_context: SecurityContext,
    /// Inbound method.
    pub method: Method,
    /// Upstream alias from the proxy URL.
    pub alias: String,
    /// Path suffix from the proxy URL.
    pub path_suffix: String,
    /// Raw query string, without the leading `?`.
    pub query: String,
    /// Inbound headers.
    pub headers: HeaderMap,
    /// Buffered request body.
    pub body: Bytes,
    /// Client address, for `scope: ip` rate limits.
    pub client_ip: Option<String>,
    /// Full request path, used as the RFC 9457 `instance`.
    pub instance: String,
    /// The connection-upgrade handle, when the inbound request has one.
    pub on_upgrade: Option<hyper::upgrade::OnUpgrade>,
}

impl std::fmt::Debug for ProxyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyRequest")
            .field("method", &self.method)
            .field("alias", &self.alias)
            .field("path_suffix", &self.path_suffix)
            .finish_non_exhaustive()
    }
}

/// The Data Plane.
pub struct DataPlane {
    control: Arc<ControlPlane>,
    registries: Arc<PluginRegistries>,
    limiter: Arc<RateLimiterRegistry>,
    metrics: Arc<OagwMetrics>,
    http: upstream_http::HttpForwarder,
    websocket: websocket::WebSocketForwarder,
    config: Arc<OagwConfig>,
    /// Per-upstream round-robin cursor for multi-endpoint pools.
    cursors: DashMap<Uuid, AtomicUsize>,
    /// In-flight request count per alias, for `oagw_requests_in_flight`.
    in_flight: DashMap<String, AtomicI64>,
}

impl std::fmt::Debug for DataPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataPlane")
    }
}

impl DataPlane {
    /// Wire the Data Plane.
    #[must_use]
    pub fn new(
        control: Arc<ControlPlane>,
        registries: Arc<PluginRegistries>,
        metrics: Arc<OagwMetrics>,
        config: Arc<OagwConfig>,
    ) -> Self {
        Self {
            control,
            registries,
            limiter: Arc::new(RateLimiterRegistry::new()),
            metrics,
            http: upstream_http::HttpForwarder::new(Arc::clone(&config)),
            websocket: websocket::WebSocketForwarder::new(Arc::clone(&config)),
            config,
            cursors: DashMap::new(),
            in_flight: DashMap::new(),
        }
    }

    /// The rate-limiter registry, exposed for the maintenance path.
    #[must_use]
    pub fn limiter(&self) -> &Arc<RateLimiterRegistry> {
        &self.limiter
    }

    /// Execute one proxy request, rendering every outcome as a response.
    pub async fn execute_proxy(&self, request: ProxyRequest) -> Response {
        let started = Instant::now();
        let instance = request.instance.clone();
        let alias = request.alias.clone();
        let method = request.method.clone();

        // A browser preflight carries no credentials, so there is no tenant
        // context to resolve an upstream with: answer it before doing
        // anything that needs one.
        if cors::is_preflight(&request.method, &request.headers) {
            return cors::preflight_response(&request.headers);
        }

        let cors_request_headers = request.headers.clone();
        self.set_in_flight(&alias, 1);
        let outcome = self.execute_inner(request).await;
        self.set_in_flight(&alias, -1);
        match outcome {
            Ok((mut response, route_pattern)) => {
                let status = response.status().as_u16();
                self.metrics.record_request(
                    &alias,
                    method.as_str(),
                    &route_pattern,
                    status,
                    started.elapsed().as_secs_f64(),
                );
                tracing::info!(
                    target: "oagw.audit",
                    event = "proxy_request",
                    host = %alias,
                    path = %instance,
                    method = %method,
                    status,
                    duration_ms = started.elapsed().as_millis(),
                    "proxied request completed"
                );
                let source = HeaderName::from_static(ERROR_SOURCE_HEADER);
                if !response.headers().contains_key(&source) {
                    response
                        .headers_mut()
                        .insert(source, HeaderValue::from_static(ERROR_SOURCE_UPSTREAM));
                }
                response
            }
            Err(err) => {
                self.metrics
                    .record_error(&alias, &instance, err.kind().gts_type().as_str());
                self.metrics.record_request(
                    &alias,
                    method.as_str(),
                    &instance,
                    err.status().as_u16(),
                    started.elapsed().as_secs_f64(),
                );
                tracing::warn!(
                    target: "oagw.audit",
                    event = "proxy_request",
                    host = %alias,
                    path = %instance,
                    method = %method,
                    status = err.status().as_u16(),
                    error_type = %err.kind().gts_type(),
                    error_message = %err.detail(),
                    duration_ms = started.elapsed().as_millis(),
                    "proxied request failed"
                );
                let mut response = err
                    .with("host", alias)
                    .with_trace_from(&cors_request_headers)
                    .into_response_with_instance(Some(&instance));
                cors::apply_response_headers(response.headers_mut(), None, &cors_request_headers);
                response
            }
        }
    }

    /// Adjust and report the in-flight gauge for one alias.
    fn set_in_flight(&self, alias: &str, delta: i64) {
        let entry = self
            .in_flight
            .entry(alias.to_owned())
            .or_insert_with(|| AtomicI64::new(0));
        let value = entry.fetch_add(delta, Ordering::Relaxed) + delta;
        self.metrics.set_in_flight(alias, value.max(0));
    }

    /// The fallible half of [`Self::execute_proxy`]. Returns the response and
    /// the route pattern to label metrics with.
    async fn execute_inner(&self, request: ProxyRequest) -> OagwResult<(Response, String)> {
        let ProxyRequest {
            security_context,
            method,
            alias,
            path_suffix,
            query,
            headers: inbound_headers,
            body,
            client_ip,
            instance,
            on_upgrade,
        } = request;

        let target = self
            .control
            .resolve_proxy_target(&security_context, &alias, method.as_str(), &path_suffix)
            .await?;
        let route_pattern = target
            .route
            .match_config
            .http
            .as_ref()
            .map_or_else(|| "/".to_owned(), |http| http.path.clone());

        // CORS on the actual request: validated after resolution, before
        // anything is forwarded.
        cors::check_actual_request(target.cors.as_ref(), &method, &inbound_headers)?;

        let request_path = normalize_request_path(&path_suffix);
        let outbound_path = self.validate_and_build_path(&target, &request_path)?;
        let outbound_query = self.validate_query(&target, &query)?;
        self.validate_body(&inbound_headers, &body)?;

        let cursor = self
            .cursors
            .entry(target.upstream.id)
            .or_insert_with(|| AtomicUsize::new(0));
        let target_host = inbound_headers
            .get(HeaderName::from_static(TARGET_HOST_HEADER))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let choice = connect::select_endpoint(&target.upstream, target_host.as_deref(), &cursor)?;
        drop(cursor);
        connect::check_transport(&self.config, &choice.endpoint)?;
        let addr = connect::resolve_endpoint(&self.config, &choice.endpoint).await?;
        self.metrics.record_endpoint_selected(
            &anonymous_id(UPSTREAM_TYPE, target.upstream.id),
            &choice.endpoint.normalized_host(),
            choice.selection,
        );

        let authority = authority_for(&choice.endpoint);
        let mut outbound_headers =
            header_rules::build_outbound_headers(&inbound_headers, &target.headers);
        header_rules::set_host(&mut outbound_headers, &authority);

        // 1. Auth plugin — one per upstream, before anything else.
        let mut outbound_query = outbound_query;
        if let Some(auth) = &target.auth {
            let mut auth_ctx = AuthContext {
                security_context: security_context.clone(),
                config: auth.config.clone(),
                headers: std::mem::take(&mut outbound_headers),
                query: std::mem::take(&mut outbound_query),
                upstream_alias: target.upstream.alias.clone(),
            };
            let plugin = self.resolve_auth_plugin(&auth.plugin_type).await?;
            let outcome = plugin.authenticate(&mut auth_ctx).await;
            outbound_headers = std::mem::take(&mut auth_ctx.headers);
            outbound_query = std::mem::take(&mut auth_ctx.query);
            outcome?;
        }

        // 2. Rate limit — the Data Plane owns the counters.
        let rate_headers =
            self.check_rate_limit(&target, &security_context, client_ip.as_deref())?;

        // 3+4. Guards, then request transforms.
        let mut request_ctx = RequestContext {
            security_context: security_context.clone(),
            method: method.clone(),
            path: outbound_path,
            query: outbound_query,
            headers: outbound_headers,
            body,
            config: crate::domain::model::PluginConfig::new(),
            upstream_alias: target.upstream.alias.clone(),
            upstream_id: target.upstream.id,
        };
        let chain = self.resolve_chain(&target.plugin_chain).await?;
        self.run_guards(&chain, &mut request_ctx).await?;
        self.run_request_transforms(&chain, &mut request_ctx)
            .await?;

        let path_and_query =
            upstream_http::build_path_and_query(&request_ctx.path, &request_ctx.query);

        // 5. Upstream. WebSocket upgrades take the splice path; everything
        // else — including SSE — takes the streaming HTTP path.
        if websocket::is_websocket_upgrade(&method, &inbound_headers) {
            let response = self
                .proxy_websocket(
                    &choice.endpoint,
                    addr,
                    &authority,
                    &path_and_query,
                    &inbound_headers,
                    &request_ctx.headers,
                    on_upgrade,
                )
                .await?;
            return Ok((response, route_pattern));
        }

        let peer = self.http.build_peer(&choice.endpoint, addr);
        let upstream_started = Instant::now();
        let upstream_response = match self
            .http
            .send(
                &peer,
                upstream_http::UpstreamRequest {
                    method: method.clone(),
                    path_and_query,
                    headers: request_ctx.headers.clone(),
                    body: request_ctx.body.clone(),
                },
            )
            .await
        {
            Ok(response) => response,
            Err(err) => {
                self.metrics.set_upstream_available(
                    &alias,
                    &choice.endpoint.normalized_host(),
                    false,
                );
                self.run_error_transforms(&chain, &err).await;
                return Err(err);
            }
        };
        self.metrics
            .set_upstream_available(&alias, &choice.endpoint.normalized_host(), true);
        self.metrics.record_phase(
            &alias,
            &route_pattern,
            "upstream",
            upstream_started.elapsed().as_secs_f64(),
        );

        // 6+7. Response guards, then response transforms.
        let mut response_ctx = ResponseContext {
            status: upstream_response.status,
            headers: header_rules::build_response_headers(
                &upstream_response.headers,
                target.headers.response.as_ref(),
            ),
            config: crate::domain::model::PluginConfig::new(),
            request_headers: request_ctx.headers.clone(),
        };
        self.run_response_guards(&chain, &mut response_ctx).await?;
        self.run_response_transforms(&chain, &mut response_ctx)
            .await?;

        let mut response = Response::new(upstream_response.into_body());
        *response.status_mut() = response_ctx.status;
        let out = response.headers_mut();
        for (name, value) in &response_ctx.headers {
            out.append(name.clone(), value.clone());
        }
        for (name, value) in rate_headers {
            out.insert(name, value);
        }
        cors::apply_response_headers(out, target.cors.as_ref(), &inbound_headers);
        out.insert(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
        let _ = instance;
        Ok((response, route_pattern))
    }

    /// Perform the WebSocket handshake and splice the two connections.
    #[allow(clippy::too_many_arguments)]
    async fn proxy_websocket(
        &self,
        endpoint: &crate::domain::model::Endpoint,
        addr: std::net::SocketAddr,
        authority: &str,
        path_and_query: &str,
        client_headers: &HeaderMap,
        forwarded: &HeaderMap,
        on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> OagwResult<Response> {
        let on_upgrade = on_upgrade.ok_or_else(|| {
            OagwError::new(
                ErrorKind::ProtocolError,
                "the inbound connection cannot be upgraded",
            )
        })?;
        let peer = self.http.build_peer(endpoint, addr);
        let outcome = self
            .websocket
            .handshake(&peer, authority, path_and_query, client_headers, forwarded)
            .await?;

        if outcome.status != StatusCode::SWITCHING_PROTOCOLS {
            // The upstream refused the upgrade: relay its own answer rather
            // than inventing a gateway error.
            return Ok(websocket::refused_handshake_response(&outcome));
        }

        let response = websocket::switching_protocols_response(&outcome.headers);
        let leftover = outcome.leftover;
        let stream = outcome.stream;
        tokio::spawn(async move {
            match on_upgrade.await {
                Ok(upgraded) => {
                    let client = hyper_util::rt::TokioIo::new(upgraded);
                    websocket::splice(client, stream, leftover).await;
                }
                Err(err) => tracing::debug!(
                    target: "oagw.websocket",
                    error = %err,
                    "inbound connection upgrade failed"
                ),
            }
        });
        Ok(response)
    }

    /// Resolve the auth plugin named by an upstream.
    async fn resolve_auth_plugin(
        &self,
        plugin_type: &str,
    ) -> OagwResult<Arc<dyn crate::domain::plugin::AuthPlugin>> {
        if let Some(plugin) = self.registries.auth.get(plugin_type) {
            return Ok(plugin);
        }
        if let Some(uuid) = plugin_ref_uuid(plugin_type) {
            // A UUID-backed auth plugin is a tenant-defined Starlark plugin.
            // The sandbox is out of scope for this build, so a bound one is a
            // resolution failure rather than a silent no-op.
            return Err(OagwError::new(
                ErrorKind::PluginNotFound,
                format!("custom auth plugin {uuid} cannot be executed by this build"),
            ));
        }
        Err(OagwError::new(
            ErrorKind::PluginNotFound,
            format!("unknown auth plugin: {plugin_type}"),
        ))
    }

    /// Resolve every chain binding into an executable plugin.
    async fn resolve_chain(&self, chain: &[PluginBinding]) -> OagwResult<Vec<ResolvedPlugin>> {
        let mut resolved = Vec::with_capacity(chain.len());
        for binding in chain {
            let kind = PluginKind::from_plugin_ref(&binding.plugin_ref);
            match kind {
                Some(PluginKind::Guard) => {
                    if let Some(plugin) = self.registries.guard.get(&binding.plugin_ref) {
                        resolved.push(ResolvedPlugin::Guard(plugin, binding.config.clone()));
                        continue;
                    }
                }
                Some(PluginKind::Transform) => {
                    if let Some(plugin) = self.registries.transform.get(&binding.plugin_ref) {
                        resolved.push(ResolvedPlugin::Transform(plugin, binding.config.clone()));
                        continue;
                    }
                }
                _ => {}
            }
            if plugin_ref_uuid(&binding.plugin_ref).is_some() {
                // Custom Starlark plugins are bindable and validated at write
                // time, but this build has no sandbox to run them in. Skip
                // rather than fail the request: the binding is legitimate.
                tracing::debug!(
                    target: "oagw.plugin",
                    plugin_ref = %binding.plugin_ref,
                    "skipping custom plugin: no Starlark sandbox in this build"
                );
                continue;
            }
            return Err(OagwError::new(
                ErrorKind::PluginNotFound,
                format!("plugin {} could not be resolved", binding.plugin_ref),
            ));
        }
        Ok(resolved)
    }

    async fn run_guards(
        &self,
        chain: &[ResolvedPlugin],
        ctx: &mut RequestContext,
    ) -> OagwResult<()> {
        for plugin in chain {
            let ResolvedPlugin::Guard(guard, config) = plugin else {
                continue;
            };
            ctx.config = config.clone();
            match guard.guard_request(ctx).await.map_err(OagwError::from)? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    error_code,
                    message,
                } => {
                    return Err(guard_rejection(status, &error_code, &message, guard.id()));
                }
            }
        }
        ctx.config = crate::domain::model::PluginConfig::new();
        Ok(())
    }

    async fn run_response_guards(
        &self,
        chain: &[ResolvedPlugin],
        ctx: &mut ResponseContext,
    ) -> OagwResult<()> {
        for plugin in chain {
            let ResolvedPlugin::Guard(guard, config) = plugin else {
                continue;
            };
            ctx.config = config.clone();
            match guard.guard_response(ctx).await.map_err(OagwError::from)? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    error_code,
                    message,
                } => {
                    return Err(guard_rejection(status, &error_code, &message, guard.id()));
                }
            }
        }
        ctx.config = crate::domain::model::PluginConfig::new();
        Ok(())
    }

    async fn run_request_transforms(
        &self,
        chain: &[ResolvedPlugin],
        ctx: &mut RequestContext,
    ) -> OagwResult<()> {
        for plugin in chain {
            let ResolvedPlugin::Transform(transform, config) = plugin else {
                continue;
            };
            if !transform.phases().contains(&"on_request") {
                continue;
            }
            ctx.config = config.clone();
            transform
                .transform_request(ctx)
                .await
                .map_err(OagwError::from)?;
        }
        ctx.config = crate::domain::model::PluginConfig::new();
        Ok(())
    }

    async fn run_response_transforms(
        &self,
        chain: &[ResolvedPlugin],
        ctx: &mut ResponseContext,
    ) -> OagwResult<()> {
        for plugin in chain {
            let ResolvedPlugin::Transform(transform, config) = plugin else {
                continue;
            };
            if !transform.phases().contains(&"on_response") {
                continue;
            }
            ctx.config = config.clone();
            transform
                .transform_response(ctx)
                .await
                .map_err(OagwError::from)?;
        }
        ctx.config = crate::domain::model::PluginConfig::new();
        Ok(())
    }

    async fn run_error_transforms(&self, chain: &[ResolvedPlugin], err: &OagwError) {
        for plugin in chain {
            let ResolvedPlugin::Transform(transform, config) = plugin else {
                continue;
            };
            if !transform.phases().contains(&"on_error") {
                continue;
            }
            let mut ctx = ErrorContext {
                message: err.to_string(),
                config: config.clone(),
            };
            if let Err(plugin_err) = transform.transform_error(&mut ctx).await {
                tracing::debug!(
                    target: "oagw.plugin",
                    error = %plugin_err,
                    "on_error transform failed"
                );
            }
        }
    }

    /// Enforce the effective rate limit and produce the `X-RateLimit-*`
    /// headers a successful request reports.
    fn check_rate_limit(
        &self,
        target: &ResolvedTarget,
        ctx: &SecurityContext,
        client_ip: Option<&str>,
    ) -> OagwResult<Vec<(HeaderName, HeaderValue)>> {
        let Some(config) = &target.rate_limit else {
            return Ok(Vec::new());
        };
        let route_pattern = target
            .route
            .match_config
            .http
            .as_ref()
            .map_or_else(|| "/".to_owned(), |http| http.path.clone());
        let key = counter_key(
            "upstream",
            &target.upstream.id.to_string(),
            config.scope,
            &ctx.subject_tenant_id().to_string(),
            &ctx.subject_id().to_string(),
            client_ip,
            &target.route.id.to_string(),
            match config.sustained.window {
                crate::domain::model::RateWindow::Second => "second",
                crate::domain::model::RateWindow::Minute => "minute",
                crate::domain::model::RateWindow::Hour => "hour",
                crate::domain::model::RateWindow::Day => "day",
            },
        );
        let decision = self.limiter.check(&key, config);
        self.metrics.set_rate_limit_usage(
            &target.upstream.alias,
            &route_pattern,
            decision.usage_ratio,
        );

        let mut out = Vec::new();
        if config.response_headers {
            push_header(&mut out, "x-ratelimit-limit", &decision.limit.to_string());
            push_header(
                &mut out,
                "x-ratelimit-remaining",
                &decision.remaining.to_string(),
            );
            push_header(
                &mut out,
                "x-ratelimit-reset",
                &decision.reset_after_secs.to_string(),
            );
        }

        if decision.allowed || !rejects_on_limit(config.strategy) {
            if !decision.allowed {
                tracing::warn!(
                    target: "oagw.audit",
                    event = "rate_limit_degraded",
                    host = %target.upstream.alias,
                    strategy = ?config.strategy,
                    "rate limit exceeded; serving under the configured non-reject strategy"
                );
            }
            return Ok(out);
        }

        self.metrics
            .record_rate_limited(&target.upstream.alias, &route_pattern);
        tracing::warn!(
            target: "oagw.audit",
            event = "rate_limit_exceeded",
            host = %target.upstream.alias,
            retry_after = decision.retry_after_secs,
            "rate limit exceeded"
        );
        let mut err = OagwError::new(
            ErrorKind::RateLimitExceeded,
            format!("Rate limit exceeded for upstream {}", target.upstream.alias),
        )
        .with_retry_after(decision.retry_after_secs)
        .with("host", target.upstream.alias.clone())
        .with(
            "upstream_id",
            anonymous_id(UPSTREAM_TYPE, target.upstream.id),
        );
        err = err.with("limit", decision.limit);
        Err(err)
    }

    /// Build the outbound path, enforcing `path_suffix_mode`.
    fn validate_and_build_path(
        &self,
        target: &ResolvedTarget,
        request_path: &str,
    ) -> OagwResult<String> {
        let Some(http) = &target.route.match_config.http else {
            return Ok(request_path.to_owned());
        };
        let remainder = path_remainder(request_path, &http.path);
        match http.path_suffix_mode {
            PathSuffixMode::Append => Ok(request_path.to_owned()),
            PathSuffixMode::Disabled => {
                if remainder.is_empty() {
                    Ok(http.path.clone())
                } else {
                    Err(OagwError::validation(format!(
                        "route {:?} has path_suffix_mode: disabled, but the request carries the \
                         suffix {remainder:?}",
                        http.path
                    ))
                    .with("path", request_path.to_owned()))
                }
            }
        }
    }

    /// Filter the query string through the route's allowlist.
    fn validate_query(
        &self,
        target: &ResolvedTarget,
        raw: &str,
    ) -> OagwResult<Vec<(String, String)>> {
        let pairs: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        if pairs.is_empty() {
            return Ok(pairs);
        }
        let Some(http) = &target.route.match_config.http else {
            return Ok(pairs);
        };
        for (key, _) in &pairs {
            if !http.query_allowlist.iter().any(|allowed| allowed == key) {
                return Err(OagwError::validation(format!(
                    "query parameter {key:?} is not in the route's query_allowlist"
                ))
                .with("query_parameter", key.clone()));
            }
        }
        Ok(pairs)
    }

    /// Default body checks: `Content-Length` well-formedness and agreement
    /// with the actual size, the hard size limit, and the transfer encoding.
    fn validate_body(&self, headers: &HeaderMap, body: &Bytes) -> OagwResult<()> {
        if let Some(raw) = headers.get(header::TRANSFER_ENCODING) {
            let value = raw.to_str().unwrap_or_default().to_ascii_lowercase();
            if value
                .split(',')
                .any(|token| !matches!(token.trim(), "chunked" | "identity" | ""))
            {
                return Err(OagwError::validation(format!(
                    "unsupported Transfer-Encoding {value:?}; only `chunked` is supported"
                )));
            }
        }
        if let Some(raw) = headers.get(header::CONTENT_LENGTH) {
            let declared: usize = raw
                .to_str()
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .ok_or_else(|| {
                    OagwError::validation("Content-Length must be a non-negative integer")
                })?;
            if declared > self.config.max_body_bytes {
                return Err(OagwError::new(
                    ErrorKind::PayloadTooLarge,
                    format!(
                        "Content-Length {declared} exceeds the {} byte limit",
                        self.config.max_body_bytes
                    ),
                ));
            }
            // Chunked bodies legitimately arrive without a matching
            // Content-Length; a declared length that disagrees with the body
            // actually received is a smuggling signature.
            if headers.get(header::TRANSFER_ENCODING).is_none() && declared != body.len() {
                return Err(OagwError::validation(format!(
                    "Content-Length {declared} does not match the {} bytes received",
                    body.len()
                )));
            }
        }
        if body.len() > self.config.max_body_bytes {
            return Err(OagwError::new(
                ErrorKind::PayloadTooLarge,
                format!(
                    "request body of {} bytes exceeds the {} byte limit",
                    body.len(),
                    self.config.max_body_bytes
                ),
            ));
        }
        Ok(())
    }
}

/// A chain entry, resolved to something executable plus its configuration.
enum ResolvedPlugin {
    Guard(
        Arc<dyn crate::domain::plugin::GuardPlugin>,
        crate::domain::model::PluginConfig,
    ),
    Transform(
        Arc<dyn crate::domain::plugin::TransformPlugin>,
        crate::domain::model::PluginConfig,
    ),
}

/// Turn a guard's rejection into the matching catalogued error.
fn guard_rejection(status: u16, error_code: &str, message: &str, plugin: &str) -> OagwError {
    let kind = match status {
        400 => ErrorKind::Validation,
        401 => ErrorKind::AuthenticationFailed,
        403 => ErrorKind::CorsOriginNotAllowed,
        413 => ErrorKind::PayloadTooLarge,
        429 => ErrorKind::RateLimitExceeded,
        502 => ErrorKind::DownstreamError,
        504 => ErrorKind::RequestTimeout,
        _ => ErrorKind::Validation,
    };
    OagwError::new(kind, message.to_owned())
        .with("error_code", error_code.to_owned())
        .with("plugin", plugin.to_owned())
}

/// Append a header pair, skipping anything unrepresentable.
fn push_header(out: &mut Vec<(HeaderName, HeaderValue)>, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        out.push((HeaderName::from_static(name), value));
    }
}

/// `host[:port]` for an endpoint, omitting the scheme's standard port.
#[must_use]
pub fn authority_for(endpoint: &crate::domain::model::Endpoint) -> String {
    let host = endpoint.normalized_host();
    let port = endpoint.port();
    if port == endpoint.scheme.default_port() {
        host
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, Scheme};

    #[test]
    fn authority_omits_the_standard_port() {
        assert_eq!(
            authority_for(&Endpoint {
                scheme: Scheme::Https,
                host: "API.OpenAI.com".to_owned(),
                port: Some(443)
            }),
            "api.openai.com"
        );
        assert_eq!(
            authority_for(&Endpoint {
                scheme: Scheme::Https,
                host: "api.openai.com".to_owned(),
                port: Some(8443)
            }),
            "api.openai.com:8443"
        );
        assert_eq!(
            authority_for(&Endpoint {
                scheme: Scheme::Http,
                host: "127.0.0.1".to_owned(),
                port: Some(80)
            }),
            "127.0.0.1"
        );
        assert_eq!(
            authority_for(&Endpoint {
                scheme: Scheme::Http,
                host: "127.0.0.1".to_owned(),
                port: Some(8080)
            }),
            "127.0.0.1:8080"
        );
    }

    #[test]
    fn guard_rejections_map_to_catalogued_errors() {
        let request_phase = guard_rejection(400, "REQUIRED_HEADER_MISSING", "missing", "g");
        assert_eq!(request_phase.status(), StatusCode::BAD_REQUEST);
        let response_phase = guard_rejection(502, "REQUIRED_HEADER_MISSING", "missing", "g");
        assert_eq!(response_phase.status(), StatusCode::BAD_GATEWAY);
        let problem = request_phase.to_problem(None);
        assert_eq!(problem["error_code"], "REQUIRED_HEADER_MISSING");
    }
}
