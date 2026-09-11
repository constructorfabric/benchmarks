//! Proxy orchestration: the data-plane service.

use std::sync::Arc;

use http::{HeaderMap, HeaderName, HeaderValue, Method};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, RateLimitConfig, Route, Upstream};
use crate::domain::plugin::{
    GuardPlugin, PluginPhase, RequestContext, ResponseContext, Trace, TransformPlugin,
};
use crate::domain::service::ControlPlaneService;
use crate::infra::http_client::OutboundClient;
use crate::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use crate::infra::proxy::cors;
use crate::infra::proxy::headers;
use crate::infra::proxy::ratelimit::{EffectiveRate, RateLimitRegistry, counter_subject};
use crate::infra::proxy::resolve::{self, Matched};
use crate::infra::proxy::uri;

/// Everything the proxy needs to execute one request.
pub struct ProxyRequest {
    /// Request method.
    pub method: Method,
    /// The path after `/proxy/`, i.e. `{alias}[/{suffix}]`.
    pub proxy_path: String,
    /// Raw query string.
    pub query: Option<String>,
    /// Inbound request headers.
    pub headers: HeaderMap,
    /// Inbound request body.
    pub body: axum::body::Body,
    /// Caller security context.
    pub security: Arc<toolkit_security::SecurityContext>,
    /// Effective tenant of the caller.
    pub tenant_id: Uuid,
    /// Caller address, used by the `ip` rate-limit scope.
    pub remote_ip: String,
    /// Shared plugin ordering sink.
    pub trace: Trace,
    /// Ancestor tenants, ordered from the direct parent to the root.
    pub ancestors: Vec<Uuid>,
}

/// Upstream response metadata kept while the body is being relayed.
#[derive(Debug, Clone)]
struct ResponseHead {
    status: http::StatusCode,
    headers: HeaderMap,
}

/// The proxy data plane.
pub struct ProxyService {
    control_plane: Arc<ControlPlaneService>,
    config: Arc<OagwConfig>,
    client: Arc<OutboundClient>,
    rate_limits: RateLimitRegistry,
    auth_plugins: AuthPluginRegistry,
    guard_plugins: GuardPluginRegistry,
    transform_plugins: TransformPluginRegistry,
}

impl ProxyService {
    /// Assemble the data plane.
    #[must_use]
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        config: Arc<OagwConfig>,
        client: Arc<OutboundClient>,
        auth_plugins: AuthPluginRegistry,
        guard_plugins: GuardPluginRegistry,
        transform_plugins: TransformPluginRegistry,
    ) -> Self {
        Self {
            control_plane,
            config,
            client,
            rate_limits: RateLimitRegistry::new(),
            auth_plugins,
            guard_plugins,
            transform_plugins,
        }
    }

    /// The control plane the data plane reads configuration from.
    #[must_use]
    pub const fn control_plane(&self) -> &Arc<ControlPlaneService> {
        &self.control_plane
    }

    /// Resolve an alias over the caller's tenant chain.
    ///
    /// # Errors
    /// Returns [`DomainError::RouteNotFound`] when no tenant in the chain owns
    /// the alias, and [`DomainError::LinkUnavailable`] when the closest
    /// upstream is disabled.
    pub async fn resolve_upstream(
        &self,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        alias: &str,
    ) -> Result<Upstream, DomainError> {
        let chain = std::iter::once(tenant_id).chain(ancestors.iter().copied());
        for tenant in chain {
            let Some(upstream) = self
                .control_plane
                .upstreams()
                .find_by_alias(tenant, alias)
                .await
                .ok()
                .flatten()
            else {
                continue;
            };
            if !upstream.enabled {
                return Err(DomainError::LinkUnavailable(format!(
                    "upstream '{alias}' is disabled"
                )));
            }
            return Ok(upstream);
        }
        Err(DomainError::RouteNotFound)
    }

    /// Routes applicable to an upstream, across the caller's tenant chain.
    async fn routes_for(
        &self,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        upstream_id: Uuid,
    ) -> Vec<Route> {
        let chain = std::iter::once(tenant_id).chain(ancestors.iter().copied());
        let mut routes = Vec::new();
        for tenant in chain {
            let Ok(tenant_routes) = self.control_plane.routes().list_by_tenant(tenant).await else {
                continue;
            };
            routes.extend(
                tenant_routes
                    .into_iter()
                    .filter(|r| r.upstream_id == upstream_id),
            );
        }
        routes
    }

    /// Execute a proxied request.
    ///
    /// # Errors
    /// Returns [`DomainError`] for every gateway-generated failure.
    pub async fn handle(
        &self,
        request: ProxyRequest,
    ) -> Result<http::Response<axum::body::Body>, DomainError> {
        let ProxyRequest {
            method,
            proxy_path,
            query,
            headers,
            body,
            security,
            tenant_id,
            remote_ip,
            trace,
            ancestors,
        } = request;
        let (alias, suffix) =
            resolve::split_alias_and_path(&proxy_path).ok_or(DomainError::RouteNotFound)?;
        let upstream = self.resolve_upstream(tenant_id, &ancestors, &alias).await?;
        let target_host = headers
            .get(crate::infra::proxy::headers::TARGET_HOST)
            .and_then(|value| value.to_str().ok());
        let target = resolve::select_endpoint(&upstream, target_host)?.clone();
        let routes = self.routes_for(tenant_id, &ancestors, upstream.id).await;
        let names = resolve::query_names(query.as_deref());
        let matched = resolve::match_route(&routes, &method, &suffix, &names)?
            .ok_or(DomainError::RouteNotFound)?;

        enforce_cors(&upstream, &matched, &method, &headers)?;
        let rate_headers = self
            .enforce_rate_limit(
                &upstream, &matched, &security, tenant_id, &ancestors, &remote_ip,
            )
            .await?;
        let request = ProxyRequest {
            method,
            proxy_path,
            query,
            headers,
            body,
            security,
            tenant_id,
            remote_ip,
            trace,
            ancestors,
        };
        self.dispatch(request, upstream, target, matched, rate_headers)
            .await
    }

    /// Check the effective rate limit and report the headers it contributes.
    ///
    /// # Errors
    /// Returns [`DomainError::RateLimitExceeded`] when the bucket is empty.
    async fn enforce_rate_limit(
        &self,
        upstream: &Upstream,
        matched: &Matched,
        security: &Arc<toolkit_security::SecurityContext>,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        remote_ip: &str,
    ) -> Result<Option<(u64, u64, u64)>, DomainError> {
        let mut configs: Vec<&RateLimitConfig> = Vec::new();
        if let Some(limit) = upstream.rate_limit.as_ref() {
            configs.push(limit);
        }
        // Ancestor upstreams are owned so their rate limits can be borrowed
        // for the remainder of the function.
        let mut inherited = Vec::new();
        for tenant in ancestors {
            if let Some(ancestor) = self
                .control_plane
                .upstreams()
                .find_by_alias(*tenant, &upstream.alias)
                .await
                .ok()
                .flatten()
                .filter(|ancestor| ancestor.enforced_rate_limit().is_some())
            {
                inherited.push(ancestor);
            }
        }
        for ancestor in &inherited {
            if let Some(limit) = ancestor.rate_limit.as_ref() {
                configs.push(limit);
            }
        }
        if let Some(limit) = matched.route.rate_limit.as_ref() {
            configs.push(limit);
        }
        let Some(effective) = EffectiveRate::merge(&configs) else {
            return Ok(None);
        };
        let subject = counter_subject(
            effective.scope,
            tenant_id,
            &security.subject_id().to_string(),
            remote_ip,
            Some(matched.route.id),
        );
        let outcome = self.rate_limits.check(
            upstream.id,
            Some(matched.route.id),
            effective.scope,
            &subject,
            &effective,
        );
        if outcome.allowed {
            return Ok(effective.response_headers.then_some((
                outcome.limit,
                outcome.remaining,
                outcome.reset_secs,
            )));
        }
        Err(DomainError::RateLimitExceeded {
            retry_after_secs: outcome.retry_after_secs,
            limit: outcome.limit,
            remaining: outcome.remaining,
            reset_secs: outcome.reset_secs,
        })
    }

    #[allow(clippy::too_many_lines, reason = "the pipeline reads as one sequence")]
    async fn dispatch(
        &self,
        request: ProxyRequest,
        upstream: Upstream,
        endpoint: Endpoint,
        matched: Matched,
        rate_headers: Option<(u64, u64, u64)>,
    ) -> Result<http::Response<axum::body::Body>, DomainError> {
        let ProxyRequest {
            method,
            proxy_path: _,
            query,
            headers,
            body,
            security,
            tenant_id,
            remote_ip: _,
            trace,
            ancestors: _,
        } = request;
        let request_rules = upstream
            .headers
            .as_ref()
            .map_or_else(Default::default, |h| h.request.clone());
        let mut request_ctx = RequestContext {
            headers: headers::build_outbound_request(&headers, &request_rules),
            path: matched.outbound_path.clone(),
            alias: upstream.alias.clone(),
            tenant_id,
            security: Arc::clone(&security),
            config: serde_json::Value::Null,
            trace: Arc::clone(&trace),
        };

        self.run_auth(&mut request_ctx, &upstream).await?;
        self.run_guards_request(&request_ctx, &upstream, &matched)
            .await?;
        self.run_transforms_request(&mut request_ctx, &upstream, &matched)
            .await?;

        let outbound_headers = request_ctx.headers.clone();
        if crate::infra::proxy::tunnel::is_upgrade_request(&method, &headers) {
            return self
                .tunnel(&request_ctx, &method, query.as_deref(), &endpoint, &matched)
                .await;
        }
        let (body, head) = self
            .call_upstream(
                &method,
                query.as_deref(),
                &headers,
                &endpoint,
                &matched,
                outbound_headers,
                body,
            )
            .await?;
        self.run_guards_response(&headers, tenant_id, &trace, &upstream, &matched, &head)
            .await?;
        build_client_response(&headers, &upstream, &matched, body, &head, rate_headers)
    }

    /// Open a raw tunnel to the upstream and hand the upgrade back to the
    /// client.
    ///
    /// The handshake is relayed byte for byte: WebSocket frames are never
    /// interpreted by the gateway. The connected upstream socket is carried in
    /// the response extensions so the handler, which owns the upgraded client
    /// socket, can start the relay.
    async fn tunnel(
        &self,
        request_ctx: &RequestContext,
        method: &http::Method,
        query: Option<&str>,
        endpoint: &Endpoint,
        matched: &Matched,
    ) -> Result<http::Response<axum::body::Body>, DomainError> {
        let host = endpoint.authority();
        let head = crate::infra::proxy::tunnel::encode_request(
            method,
            &crate::infra::proxy::tunnel::with_query(&matched.outbound_path, query),
            &host,
            &request_ctx.headers,
        );
        let handshake =
            crate::infra::proxy::tunnel::dial(endpoint, &head, self.config.proxy_timeout()).await?;
        let status = crate::infra::proxy::tunnel::switching_protocols();
        if handshake.status != status.as_u16() {
            return Err(DomainError::ProtocolError(format!(
                "upstream did not upgrade the connection (status {})",
                handshake.status
            )));
        }
        let (parts, _body) = http::Response::builder()
            .status(status)
            .body(())
            .map_err(|_| {
                DomainError::ProtocolError("failed to build the tunnel response".to_owned())
            })?
            .into_parts();
        let mut response = http::Response::from_parts(parts, axum::body::Body::empty());
        for (name, value) in crate::infra::proxy::tunnel::relay_headers(&handshake.headers) {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        response
            .extensions_mut()
            .insert(crate::infra::proxy::tunnel::TunnelHandle::new(
                handshake.stream,
            ));
        Ok(response)
    }

    async fn run_auth(
        &self,
        ctx: &mut RequestContext,
        upstream: &Upstream,
    ) -> Result<(), DomainError> {
        let Some(auth) = upstream.auth.as_ref() else {
            return Ok(());
        };
        let Some(plugin) = self.auth_plugins.get(&auth.auth_type) else {
            return Err(DomainError::PluginNotFound(format!(
                "auth plugin '{}' is not resolvable",
                auth.auth_type
            )));
        };
        ctx.config = auth.config.clone();
        plugin.authenticate(ctx).await.map_err(plugin_to_domain)
    }

    async fn run_guards_request(
        &self,
        ctx: &RequestContext,
        upstream: &Upstream,
        matched: &resolve::Matched,
    ) -> Result<(), DomainError> {
        for (guard, config) in self.guard_chain(upstream, matched) {
            let scoped = RequestContext {
                headers: ctx.headers.clone(),
                path: ctx.path.clone(),
                alias: ctx.alias.clone(),
                tenant_id: ctx.tenant_id,
                security: Arc::clone(&ctx.security),
                config,
                trace: Arc::clone(&ctx.trace),
            };
            guard
                .guard_request(&scoped)
                .await
                .map_err(plugin_to_domain)?;
        }
        Ok(())
    }

    async fn run_transforms_request(
        &self,
        ctx: &mut RequestContext,
        upstream: &Upstream,
        matched: &Matched,
    ) -> Result<(), DomainError> {
        for (transform, config) in self.transform_chain(upstream, matched) {
            ctx.config = config;
            transform
                .transform_request(ctx)
                .await
                .map_err(plugin_to_domain)?;
        }
        ctx.record("pipeline", PluginPhase::Request);
        Ok(())
    }

    async fn run_guards_response(
        &self,
        inbound_headers: &HeaderMap,
        tenant_id: Uuid,
        trace: &Trace,
        upstream: &Upstream,
        matched: &Matched,
        head: &ResponseHead,
    ) -> Result<(), DomainError> {
        for (guard, config) in self.guard_chain(upstream, matched) {
            let ctx = ResponseContext {
                status: head.status,
                headers: head.headers.clone(),
                alias: upstream.alias.clone(),
                tenant_id,
                config,
                trace: Arc::clone(trace),
            };
            guard.guard_response(&ctx).await.map_err(plugin_to_domain)?;
        }
        let _ = inbound_headers;
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "each parameter is a distinct facet of the one upstream call"
    )]
    async fn call_upstream(
        &self,
        method: &http::Method,
        query: Option<&str>,
        inbound_headers: &HeaderMap,
        endpoint: &Endpoint,
        matched: &Matched,
        outbound_headers: HeaderMap,
        body: axum::body::Body,
    ) -> Result<(axum::body::Body, ResponseHead), DomainError> {
        let max = self.config.max_request_body_bytes;
        let (bytes, size) = crate::infra::proxy::body::read_bounded(body, max).await?;
        crate::infra::proxy::body::validate(inbound_headers, size, max)?;

        let uri = uri::build_uri(endpoint, &matched.outbound_path, query)?;
        let mut builder = http::Request::builder().method(method.clone()).uri(uri);
        for (name, value) in &outbound_headers {
            builder = builder.header(name, value);
        }
        if let Ok(host) = HeaderValue::from_str(&endpoint.authority()) {
            builder = builder.header(http::header::HOST, host);
        }
        let outbound = builder.body(axum::body::Body::from(bytes)).map_err(|_| {
            DomainError::ProtocolError("failed to build the outbound request".to_owned())
        })?;

        let future = self.client.request(outbound);
        match tokio::time::timeout(self.config.proxy_timeout(), future).await {
            Ok(Ok(response)) => {
                let status = response.status();
                let response_headers = response.headers().clone();
                let body = axum::body::Body::new(response.into_body());
                Ok((
                    body,
                    ResponseHead {
                        status,
                        headers: response_headers,
                    },
                ))
            }
            Ok(Err(err)) => Err(transport_error(&err.to_string())),
            Err(_) => Err(DomainError::RequestTimeout),
        }
    }

    /// Guards bound on the upstream followed by those bound on the route.
    #[must_use]
    pub fn guard_chain(
        &self,
        upstream: &Upstream,
        matched: &Matched,
    ) -> Vec<(Arc<dyn GuardPlugin>, serde_json::Value)> {
        chained(|id| self.guard_plugins.get(id), upstream, matched)
    }

    /// Transforms bound on the upstream followed by those bound on the route.
    #[must_use]
    pub fn transform_chain(
        &self,
        upstream: &Upstream,
        matched: &Matched,
    ) -> Vec<(Arc<dyn TransformPlugin>, serde_json::Value)> {
        chained(|id| self.transform_plugins.get(id), upstream, matched)
    }

    /// The token-bucket registry.
    #[must_use]
    pub const fn limiter(&self) -> &RateLimitRegistry {
        &self.rate_limits
    }

    /// The auth-plugin registry.
    #[must_use]
    pub const fn auths(&self) -> &AuthPluginRegistry {
        &self.auth_plugins
    }

    /// The guard-plugin registry.
    #[must_use]
    pub const fn guards(&self) -> &GuardPluginRegistry {
        &self.guard_plugins
    }

    /// The transform-plugin registry.
    #[must_use]
    pub const fn transforms(&self) -> &TransformPluginRegistry {
        &self.transform_plugins
    }

    /// The configured proxy timeout.
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        self.config.proxy_timeout()
    }

    /// The configured request-body ceiling.
    #[must_use]
    pub fn max_request_body_bytes(&self) -> u64 {
        self.config.max_request_body_bytes
    }
}

/// Compose the client-facing response from the upstream head and the gear
/// configuration.
fn build_client_response(
    inbound_headers: &HeaderMap,
    upstream: &Upstream,
    matched: &Matched,
    body: axum::body::Body,
    head: &ResponseHead,
    rate_headers: Option<(u64, u64, u64)>,
) -> Result<http::Response<axum::body::Body>, DomainError> {
    let mut out = match upstream.headers.as_ref() {
        Some(config) => headers::build_outbound_response(&head.headers, &config.response),
        None => head.headers.clone(),
    };
    let cors_config =
        crate::domain::model::effective_cors(upstream.cors.as_ref(), matched.route.cors.as_ref());
    if let Some(config) = cors_config
        && config.enabled
    {
        for (name, value) in &cors::actual_response_headers(config, inbound_headers) {
            out.insert(name, value.clone());
        }
    }
    if let Ok(name) = HeaderName::from_bytes(headers::ERROR_SOURCE.as_bytes()) {
        out.insert(name, HeaderValue::from_static(cors::UPSTREAM_SOURCE));
    }
    out.remove(http::header::CONTENT_LENGTH);

    let mut builder = http::Response::builder().status(head.status);
    if let Some((limit, remaining, reset)) = rate_headers {
        builder = builder
            .header(headers::RATE_LIMIT, limit)
            .header(headers::RATE_REMAINING, remaining)
            .header(headers::RATE_RESET, reset);
    }
    for (name, value) in &out {
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        .map_err(|_| DomainError::ProtocolError("failed to build the client response".to_owned()))
}

/// Enforce the CORS configuration in force for the request.
///
/// # Errors
/// Returns [`DomainError::CorsOriginNotAllowed`] or
/// [`DomainError::CorsMethodNotAllowed`] on a violation.
fn enforce_cors(
    upstream: &Upstream,
    matched: &Matched,
    method: &Method,
    inbound_headers: &HeaderMap,
) -> Result<(), DomainError> {
    let config =
        crate::domain::model::effective_cors(upstream.cors.as_ref(), matched.route.cors.as_ref());
    if !cors::applies(config, inbound_headers) {
        return Ok(());
    }
    let Some(config) = config else {
        return Ok(());
    };
    cors::validate_actual(config, method, inbound_headers)
}

/// Map a plugin failure onto the domain error table.
fn plugin_to_domain(err: crate::domain::plugin::PluginError) -> DomainError {
    match err.kind {
        crate::domain::plugin::PluginErrorKind::Unresolvable => {
            DomainError::PluginNotFound(err.detail)
        }
        crate::domain::plugin::PluginErrorKind::Authentication => {
            DomainError::AuthenticationFailed(err.detail)
        }
        crate::domain::plugin::PluginErrorKind::Upstream => {
            DomainError::DownstreamError(err.detail)
        }
        crate::domain::plugin::PluginErrorKind::BadRequest => {
            <DomainError as From<&crate::domain::plugin::PluginError>>::from(&err)
        }
    }
}

/// Transport failures mapped onto the DESIGN error table.
fn transport_error(detail: &str) -> DomainError {
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("timed out") || lowered.contains("timeout") {
        DomainError::ConnectionTimeout
    } else if lowered.contains("refused")
        || lowered.contains("closed")
        || lowered.contains("reset")
        || lowered.contains("connect")
    {
        DomainError::LinkUnavailable(detail.to_owned())
    } else {
        DomainError::DownstreamError(detail.to_owned())
    }
}

/// Resolve the plugins bound on an upstream followed by those on the route.
fn chained<P: Clone + 'static>(
    lookup: impl Fn(&str) -> Option<P>,
    upstream: &Upstream,
    matched: &Matched,
) -> Vec<(P, serde_json::Value)> {
    let mut out = Vec::new();
    for chain in [upstream.plugins.as_ref(), matched.route.plugins.as_ref()] {
        let Some(plugins) = chain else { continue };
        for binding in &plugins.items {
            if let Some(plugin) = lookup(&binding.plugin_ref) {
                out.push((plugin, binding.config.clone()));
            }
        }
    }
    out
}
