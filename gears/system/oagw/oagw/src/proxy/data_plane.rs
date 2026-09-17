//! Data plane: the proxy execution pipeline.
//!
//! Pipeline (ADR 0002 execution order, upstream plugins before route plugins):
//!
//! 1. tenant chain walk → alias resolution (closest match wins, shadowing)
//! 2. route match (method allowlist, longest path prefix, suffix + query rules)
//! 3. CORS actual-request enforcement (ADR 0004)
//! 4. hierarchical rate limit → 429 (ADR 0003)
//! 5. body-size ceiling → 413
//! 6. endpoint selection, plaintext gate, SSRF segment check
//! 7. Auth → Guards(request) → Transforms(request)
//! 8. upstream call (streaming) under the `proxy_timeout_secs` ceiling → 504
//! 9. Transforms(response) → Guards(response)
//!
//! Every failure raised here is a [`DomainError`], rendered as RFC 9457
//! problem+json with `X-OAGW-Error-Source: gateway`. Upstream failures are
//! streamed through untouched and only stamped `X-OAGW-Error-Source: upstream`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use dashmap::DashMap;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use hyper_util::rt::TokioIo;
use pingora_core::connectors::TransportConnector;
use pingora_core::upstreams::peer::HttpPeer;
use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::gts_helpers::{ERROR_SOURCE_HEADER, TARGET_HOST_HEADER};

/// `X-OAGW-Error-Source` value for a passthrough response.
const UPSTREAM_SOURCE: &str = "upstream";
use crate::domain::model::{
    CorsConfig, Endpoint, HttpMatch, PathSuffixMode, RateLimitConfig, Route, SharingMode, Upstream,
};
use crate::domain::plugin::{
    AuthPlugin, GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext,
    TransformPlugin,
};
use crate::domain::repo::{RouteRepository, UpstreamRepository};
use crate::infra::state::GearState;
use crate::proxy::cors;
use crate::proxy::headers::{
    apply_header_rules, apply_passthrough_policy, headers_from_pairs, is_upgrade_request,
    pairs_from_headers, pairs_from_query, resolve_target_host, strip_hop_by_hop,
    strip_hop_by_hop_keep_upgrade, strip_routing_headers,
};
use crate::proxy::ratelimit::{RateDecision, SharedRateLimiter, bucket_key, merge_limits};

/// Transport-agnostic upstream connection handle.
///
/// Both transports are wrapped in [`TokioIo`], which implements hyper's
/// `rt::Read`/`rt::Write` for tokio IO — the bounds [`hyper::client`] needs.
trait UpstreamIo: hyper::rt::Read + hyper::rt::Write + Unpin + Send {}

impl<T: hyper::rt::Read + hyper::rt::Write + Unpin + Send> UpstreamIo for T {}

/// Upstream connection handed to the HTTP/1.1 client.
type BoxedIo = Box<dyn UpstreamIo>;

/// Everything the data plane needs to execute one proxy request.
pub struct ProxyRequest {
    /// Normalized upstream alias from the path.
    pub alias: String,
    /// Raw path suffix after `/proxy/{alias}` (empty or `/`-prefixed).
    pub path_suffix: String,
    /// Inbound request. Routing and hop-by-hop headers are still present; the
    /// body is streamed.
    pub request: http::Request<Body>,
    /// Peer address as text; used by the `ip` rate-limit scope.
    pub client_ip: String,
    /// Caller identity; required when an auth plugin is bound.
    pub security_context: Option<SecurityContext>,
}

/// `X-RateLimit-*` values to stamp on a successful response.
#[derive(Debug, Clone, Copy)]
struct RateHeaders {
    limit: u32,
    remaining: u32,
    reset_secs: u32,
}

/// Control-plane configuration resolved for one proxy request.
struct ResolvedConfig {
    upstream: Upstream,
    /// `enforce`-shared ancestor rate limits; shadowing cannot bypass them.
    ancestor_rate_limits: Vec<RateLimitConfig>,
    /// Ancestor CORS blocks shared `inherit`/`enforce`.
    ancestor_cors: Vec<CorsConfig>,
    /// Routes of the resolved upstream visible from the calling tenant chain,
    /// descendants first.
    routes: Vec<Route>,
}

/// One resolved plugin binding: the shared plugin and its configured options.
type Binding<P> = (Arc<P>, BTreeMap<String, String>);

/// Resolved plugin chain: upstream bindings first, then route bindings.
struct ChainPlan {
    auth: Option<Binding<dyn AuthPlugin>>,
    guards: Vec<Binding<dyn GuardPlugin>>,
    transforms: Vec<Binding<dyn TransformPlugin>>,
}

/// Executes the proxy pipeline.
pub struct DataPlane {
    state: GearState,
    limiter: SharedRateLimiter,
    round_robin: DashMap<uuid::Uuid, AtomicUsize>,
    tls: OnceLock<TransportConnector>,
}

impl std::fmt::Debug for DataPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlane").finish_non_exhaustive()
    }
}

impl DataPlane {
    /// Builds a data plane over the shared gear state.
    #[must_use]
    pub fn new(state: GearState, limiter: SharedRateLimiter) -> Self {
        Self {
            state,
            limiter,
            round_robin: DashMap::new(),
            tls: OnceLock::new(),
        }
    }

    /// The gear configuration in force.
    #[must_use]
    pub fn config(&self) -> Arc<OagwConfig> {
        self.state.config.clone()
    }

    /// Runs the pipeline.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] for every gateway-originated failure.
    pub async fn proxy(
        &self,
        security_context: &SecurityContext,
        request: ProxyRequest,
    ) -> Result<axum::response::Response, DomainError> {
        let instance = format!("/oagw/v1/proxy/{}", request.alias);
        let chain = self
            .state
            .tenants
            .chain(security_context)
            .await
            .map_err(|err| err.with_instance(instance.clone()))?;
        let config = self
            .resolve_config(&chain, &request.alias)
            .await
            .map_err(|err| err.with_instance(instance.clone()))?;
        let route = self.select_route(&config, &request, &instance)?;
        let origin = cors::header_value(request.request.headers(), "origin").map(str::to_owned);
        let effective_cors = merge_cors(&config.upstream.cors, &config.ancestor_cors);
        cors::check_actual_request(&effective_cors, origin.as_deref(), request.request.method())
            .map_err(|err| err.with_instance(instance.clone()))?;

        let rate_headers = self.apply_rate_limit(&config, route, security_context, &request)?;
        let index = self.select_endpoint_index(&config.upstream, &request, &instance)?;
        self.check_body_limits(request.request.headers(), &instance)?;
        self.check_protocol_policy(&config.upstream, index, &instance)?;

        let plan = build_chain_plan(&self.state, &config.upstream, route);
        let mut context =
            build_request_context(security_context, &config.upstream, index, &request, route);
        run_request_plugins(&plan, self.state.secrets.as_ref(), &mut context).await?;

        let max_body = self.state.config.max_body_bytes;
        self.forward(
            request,
            &config.upstream,
            index,
            &effective_cors,
            origin.as_deref(),
            plan,
            context,
            rate_headers,
            max_body,
        )
        .await
    }

    // ---------------------------------------------------------------------------------
    // Control-plane resolution
    // ---------------------------------------------------------------------------------

    /// Resolves the upstream and its visible routes.
    ///
    /// The tenant chain is walked descendant → root: the closest *enabled*
    /// upstream carrying `alias` wins. Ancestors that declare the same alias
    /// still contribute their `enforce`-shared rate limits and CORS blocks,
    /// which shadowing can never bypass.
    async fn resolve_config(
        &self,
        chain: &[uuid::Uuid],
        alias: &str,
    ) -> Result<ResolvedConfig, DomainError> {
        let mut selected: Option<Upstream> = None;
        let mut ancestor_rate_limits: Vec<RateLimitConfig> = Vec::new();
        let mut ancestor_cors: Vec<CorsConfig> = Vec::new();

        for tenant in chain {
            let Some(found) = self.state.store.find_by_alias(*tenant, alias).await? else {
                continue;
            };
            if selected.is_none() {
                selected = Some(found);
                continue;
            }
            if let Some(limit) = enforced_limit(found.rate_limit.as_ref()) {
                ancestor_rate_limits.push(limit);
            }
            if found.cors.enabled && !found.cors.allowed_origins.is_empty() {
                ancestor_cors.push(found.cors);
            }
        }

        let upstream = selected.ok_or_else(|| {
            DomainError::new(
                ErrorKind::RouteNotFound,
                format!("no upstream matches alias '{alias}'"),
            )
        })?;
        if !upstream.enabled {
            return Err(DomainError::new(
                ErrorKind::LinkUnavailable,
                format!("upstream '{}' is disabled", upstream.alias),
            ));
        }

        let routes = self
            .state
            .store
            .list_by_tenants(chain, &Default::default())
            .await?
            .into_iter()
            .filter(|route| route.upstream_id == upstream.id)
            .collect();

        Ok(ResolvedConfig {
            upstream,
            ancestor_rate_limits,
            ancestor_cors,
            routes,
        })
    }

    /// Longest-prefix route match across the tenant chain (descendants first).
    fn select_route<'a>(
        &self,
        config: &'a ResolvedConfig,
        request: &ProxyRequest,
        instance: &str,
    ) -> Result<Option<&'a Route>, DomainError> {
        let method = request.request.method();
        let matched = config
            .routes
            .iter()
            .filter(|route| route.enabled)
            .filter(|route| route_matches(route, method, &request.path_suffix))
            .max_by_key(|route| http_match(route).map_or(0, |rule| rule.path.len()));

        if let Some(route) = matched {
            if http_match(route)
                .is_some_and(|rule| rule.path_suffix_mode == PathSuffixMode::Disabled)
                && path_remainder(route, &request.path_suffix).is_some_and(|rest| !rest.is_empty())
            {
                return Err(DomainError::new(
                    ErrorKind::Validation,
                    "path suffix is not permitted by the matched route",
                )
                .with_instance(instance.to_owned()));
            }
            self.check_query_allowlist(route, request, instance)?;
            return Ok(Some(route));
        }
        Err(DomainError::new(
            ErrorKind::RouteNotFound,
            format!(
                "no route matches {} {} for alias '{}'",
                method, request.path_suffix, request.alias
            ),
        )
        .with_instance(instance.to_owned()))
    }

    /// Rejects query parameters outside the route's allow-list.
    fn check_query_allowlist(
        &self,
        route: &Route,
        request: &ProxyRequest,
        instance: &str,
    ) -> Result<(), DomainError> {
        let Some(allowlist) = http_match(route)
            .map(|rule| &rule.query_allowlist)
            .filter(|list| !list.is_empty())
        else {
            return Ok(());
        };
        let query = request.request.uri().query().unwrap_or_default();
        for (name, _) in pairs_from_query(query) {
            let known = allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&name));
            if !known {
                return Err(DomainError::new(
                    ErrorKind::Validation,
                    format!("query parameter '{name}' is not allowed by the matched route"),
                )
                .with_instance(instance.to_owned())
                .with_extension("query_parameter", serde_json::json!(name)));
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------
    // Pre-flight guards
    // ---------------------------------------------------------------------------------

    /// Applies the merged hierarchical rate limit, returning the response headers.
    fn apply_rate_limit(
        &self,
        config: &ResolvedConfig,
        route: Option<&Route>,
        security_context: &SecurityContext,
        request: &ProxyRequest,
    ) -> Result<Option<RateHeaders>, DomainError> {
        let Some(limit) = effective_rate_limit(
            config.upstream.rate_limit.as_ref(),
            &config.ancestor_rate_limits,
            route.and_then(|route| route.rate_limit.as_ref()),
        )
        .filter(|limit| limit.enabled) else {
            return Ok(None);
        };
        let route_id = route.map_or_else(uuid::Uuid::nil, |route| route.id);
        let key = bucket_key(
            "upstream",
            limit.scope,
            &config.upstream.tenant_id,
            &security_context.subject_id(),
            &request.client_ip,
            &route_id,
        );
        match self.limiter.consume(&key, &limit, limit.cost) {
            RateDecision::Allowed {
                remaining,
                reset_secs,
            } => Ok(Some(RateHeaders {
                limit: limit.capacity(),
                remaining,
                reset_secs,
            })),
            RateDecision::Limited { retry_after_secs } => Err(DomainError::new(
                ErrorKind::RateLimitExceeded,
                "token bucket exhausted for this scope",
            )
            .with_retry_after(u64::from(retry_after_secs))),
        }
    }

    /// Round-robin slot for an unspecified multi-endpoint pool.
    fn select_endpoint_index(
        &self,
        upstream: &Upstream,
        request: &ProxyRequest,
        instance: &str,
    ) -> Result<usize, DomainError> {
        let pinned = target_host(request.request.headers());
        resolve_target_host(upstream, pinned.as_deref())
            .map_err(|err| err.with_instance(instance.to_owned()))?;
        let len = upstream.server.endpoints.len();
        if pinned.is_some() || len <= 1 {
            return Ok(0);
        }
        let counter = self.round_robin.entry(upstream.id).or_default();
        let index = counter.fetch_add(1, Ordering::Relaxed);
        Ok(index % len)
    }

    /// Enforces the 100 MB (configurable) body ceiling before buffering.
    fn check_body_limits(&self, headers: &HeaderMap, instance: &str) -> Result<(), DomainError> {
        let limit = self.state.config.max_body_bytes;
        if let Some(length) = content_length(headers)
            && length > limit
        {
            return Err(DomainError::new(
                ErrorKind::PayloadTooLarge,
                format!("request body of {length} bytes exceeds the {limit} byte ceiling"),
            )
            .with_instance(instance.to_owned()));
        }
        if !transfer_encoding_supported(headers) {
            return Err(DomainError::new(
                ErrorKind::Validation,
                "unsupported Transfer-Encoding (only 'chunked' is supported)",
            )
            .with_instance(instance.to_owned()));
        }
        Ok(())
    }

    /// Scheme and SSRF posture of the selected endpoint.
    fn check_protocol_policy(
        &self,
        upstream: &Upstream,
        index: usize,
        instance: &str,
    ) -> Result<(), DomainError> {
        let endpoint = &upstream.server.endpoints[index];
        enforce_plaintext_policy(&self.state.config, endpoint)
            .map_err(|err| err.with_instance(instance.to_owned()))?;
        enforce_ssrf_policy(&self.state.config, endpoint)
            .map_err(|err| err.with_instance(instance.to_owned()))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------------
    // Upstream call and response pipeline
    // ---------------------------------------------------------------------------------

    /// Lazily builds the TLS connector; plaintext proxying never touches it.
    fn tls_connector(&self) -> &TransportConnector {
        self.tls.get_or_init(|| TransportConnector::new(None))
    }

    /// Opens the upstream transport under the proxy timeout ceiling.
    async fn connect(&self, endpoint: &Endpoint, instance: &str) -> Result<BoxedIo, DomainError> {
        let host = endpoint.host.clone();
        let port = endpoint.effective_port();
        let timeout = self.state.config.proxy_timeout();
        if endpoint.scheme.is_plaintext() {
            let stream = tokio::time::timeout(
                timeout,
                tokio::net::TcpStream::connect((host.as_str(), port)),
            )
            .await
            .map_err(|_| timeout_error(ErrorKind::ConnectionTimeout, instance))?
            .map_err(|_| connect_error(instance))?;
            return Ok(Box::new(TokioIo::new(stream)));
        }
        let addr = tokio::time::timeout(timeout, tokio::net::lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| timeout_error(ErrorKind::ConnectionTimeout, instance))?
            .map_err(|_| connect_error(instance))?
            .next()
            .ok_or_else(|| connect_error(instance))?;
        let peer = HttpPeer::new(addr, true, host);
        let stream = tokio::time::timeout(timeout, self.tls_connector().new_stream(&peer))
            .await
            .map_err(|_| timeout_error(ErrorKind::ConnectionTimeout, instance))?
            .map_err(|_| connect_error(instance))?;
        Ok(Box::new(TokioIo::new(stream)))
    }

    /// Connects, sends the assembled request and renders the response.
    #[allow(
        clippy::too_many_arguments,
        reason = "linear pipeline hand-off between the pre-flight and render phases"
    )]
    async fn forward(
        &self,
        mut request: ProxyRequest,
        upstream: &Upstream,
        index: usize,
        cors_config: &CorsConfig,
        origin: Option<&str>,
        plan: ChainPlan,
        context: RequestContext,
        rate_headers: Option<RateHeaders>,
        max_body: u64,
    ) -> Result<axum::response::Response, DomainError> {
        let instance = format!("/oagw/v1/proxy/{}", request.alias);
        let endpoint = &upstream.server.endpoints[index];
        let upgrade = is_upgrade_request(request.request.headers());
        let body = std::mem::replace(request.request.body_mut(), Body::empty());
        let outbound = build_outbound_request(
            request.request.method(),
            upstream,
            endpoint,
            context,
            upgrade,
            limited_body(body, max_body),
        )?;
        let io = self.connect(endpoint, &instance).await?;

        let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|_| connect_error(&instance))?;
        tokio::spawn(async move {
            if connection.with_upgrades().await.is_err() {
                tracing::debug!("upstream connection task terminated");
            }
        });

        let response = tokio::time::timeout(
            self.state.config.proxy_timeout(),
            sender.send_request(outbound),
        )
        .await
        .map_err(|_| timeout_error(ErrorKind::RequestTimeout, &instance))?
        .map_err(|_| upstream_error(&instance))?;

        if upgrade && response.status() == StatusCode::SWITCHING_PROTOCOLS {
            return self.establish_upgrade(request, response, &instance).await;
        }
        self.render_response(
            response,
            upstream,
            &plan,
            cors_config,
            origin,
            rate_headers,
            &instance,
        )
        .await
    }

    /// Splices a negotiated upstream upgrade into the caller's socket.
    ///
    /// The caller's socket is handed over only once the `101` below has been
    /// written, so the client leg of the splice is awaited in a task that
    /// outlives this handler.
    async fn establish_upgrade(
        &self,
        request: ProxyRequest,
        mut response: http::Response<hyper::body::Incoming>,
        instance: &str,
    ) -> Result<axum::response::Response, DomainError> {
        let ceiling = self.state.config.proxy_timeout();
        // The handshake headers must reach the caller before the socket is
        // handed over: without `Upgrade`/`Connection`/`Sec-WebSocket-Accept`
        // the client cannot complete the protocol switch.
        let handshake = upgrade_handshake(response.headers());
        let upstream_io = tokio::time::timeout(ceiling, hyper::upgrade::on(&mut response))
            .await
            .map_err(|_| timeout_error(ErrorKind::ConnectionTimeout, instance))?
            .map_err(|_| connect_error(instance))?;
        tokio::spawn(async move {
            let mut request = request;
            let client_io =
                tokio::time::timeout(ceiling, hyper::upgrade::on(&mut request.request)).await;
            let client_io = match client_io {
                Ok(Ok(io)) => io,
                Ok(Err(_)) | Err(_) => {
                    tracing::debug!("caller side of the upgrade never materialised");
                    return;
                }
            };
            let mut client_io = TokioIo::new(client_io);
            let mut upstream_io = TokioIo::new(upstream_io);
            if tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io)
                .await
                .is_err()
            {
                tracing::debug!("upgraded stream terminated");
            }
        });
        let mut builder =
            axum::response::Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
        for (name, value) in handshake {
            builder = builder.header(name, value);
        }
        builder
            .body(Body::empty())
            .map_err(|_| DomainError::from(ErrorKind::ProtocolError))
    }

    /// Assembles the caller-facing response, streaming the upstream body.
    #[allow(
        clippy::too_many_arguments,
        reason = "linear pipeline hand-off between the upstream call and rendering"
    )]
    async fn render_response(
        &self,
        response: http::Response<hyper::body::Incoming>,
        upstream: &Upstream,
        plan: &ChainPlan,
        cors_config: &CorsConfig,
        origin: Option<&str>,
        rate_headers: Option<RateHeaders>,
        instance: &str,
    ) -> Result<axum::response::Response, DomainError> {
        let (parts, body) = response.into_parts();
        let status = parts.status;
        let mut headers = parts.headers;
        strip_hop_by_hop(&mut headers);
        strip_routing_headers(&mut headers);

        let mut pairs = pairs_from_headers(&headers);
        for (guard, config) in &plan.guards {
            let response_context = ResponseContext {
                status: status.as_u16(),
                headers: pairs.clone(),
                config: config.clone(),
            };
            match guard.guard_response(&response_context).await {
                Ok(decision) => ensure_allowed(&decision, instance)?,
                Err(err) => return Err(plugin_failure(&err, ErrorKind::ProtocolError, instance)),
            }
            pairs = response_context.headers;
        }
        for (transform, config) in &plan.transforms {
            let mut response_context = ResponseContext {
                status: status.as_u16(),
                headers: pairs.clone(),
                config: config.clone(),
            };
            transform
                .transform_response(&mut response_context)
                .await
                .map_err(|err| plugin_failure(&err, ErrorKind::ProtocolError, instance))?;
            pairs = response_context.headers;
        }
        let mut headers = headers_from_pairs(&pairs);
        apply_header_rules(&mut headers, &upstream.headers.response);
        apply_passthrough_policy(&mut headers, &upstream.headers.response);
        if let Some(origin) = origin {
            cors::apply_response_headers(&mut headers, origin, cors_config);
        }
        if let Some(rate) = rate_headers {
            stamp_rate_headers(&mut headers, rate);
        }
        if let Ok(name) = http::HeaderName::from_bytes(ERROR_SOURCE_HEADER.as_bytes()) {
            headers.insert(name, HeaderValue::from_static(UPSTREAM_SOURCE));
        }
        let builder = axum::response::Response::builder().status(status);
        let mut builder = builder;
        for (name, value) in &headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(name.as_str().as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                builder = builder.header(name, value);
            }
        }
        builder
            .body(Body::new(body))
            .map_err(|_| DomainError::from(ErrorKind::ProtocolError))
    }
}

// -------------------------------------------------------------------------------------
// Plugin pipeline
// -------------------------------------------------------------------------------------

/// Auth → Guards(request) → Transforms(request).
async fn run_request_plugins(
    plan: &ChainPlan,
    secrets: &dyn crate::domain::plugin::SecretResolver,
    context: &mut RequestContext,
) -> Result<(), DomainError> {
    if let Some((plugin, config)) = &plan.auth {
        context.config = config.clone();
        match plugin.authenticate(context, secrets).await {
            Ok(()) => (),
            Err(err) => return Err(auth_failure(&err)),
        }
    }
    for (guard, config) in &plan.guards {
        context.config = config.clone();
        match guard.guard_request(context).await {
            Ok(decision) => ensure_allowed(&decision, "")?,
            Err(err) => return Err(plugin_failure(&err, ErrorKind::Validation, "")),
        }
    }
    for (transform, config) in &plan.transforms {
        context.config = config.clone();
        transform
            .transform_request(context)
            .await
            .map_err(|err| plugin_failure(&err, ErrorKind::Validation, ""))?;
    }
    Ok(())
}

/// Builds the plugin-visible request context for the selected endpoint.
fn build_request_context(
    security_context: &SecurityContext,
    upstream: &Upstream,
    index: usize,
    request: &ProxyRequest,
    route: Option<&Route>,
) -> RequestContext {
    let endpoint = &upstream.server.endpoints[index];
    let mut headers = request.request.headers().clone();
    if is_upgrade_request(&headers) {
        strip_hop_by_hop_keep_upgrade(&mut headers);
    } else {
        strip_hop_by_hop(&mut headers);
    }
    strip_routing_headers(&mut headers);
    if let Some(host) = host_value(endpoint)
        && let Ok(value) = HeaderValue::from_str(&host)
    {
        headers.insert(header::HOST, value);
    }
    let query = request.request.uri().query().unwrap_or_default();
    let mode = route
        .and_then(http_match)
        .map_or(PathSuffixMode::Append, |rule| rule.path_suffix_mode);
    let suffix = if mode == PathSuffixMode::Append {
        // Only the part of the suffix the route prefix does not already cover
        // is appended: a route matching `/v1` reached with `/v1/pets` must
        // still target `/v1/pets`, not `/v1/v1/pets`.
        route
            .and_then(|route| path_remainder(route, &request.path_suffix))
            .unwrap_or_else(|| request.path_suffix.clone())
    } else {
        String::new()
    };

    RequestContext {
        security_context: Some(security_context.clone()),
        headers: pairs_from_headers(&headers),
        query: pairs_from_query(query),
        path: outbound_path(route, &suffix),
        config: BTreeMap::new(),
        tenant_id: Some(upstream.tenant_id),
    }
}

/// Resolves the plugin chain: upstream bindings first, then route bindings.
fn build_chain_plan(state: &GearState, upstream: &Upstream, route: Option<&Route>) -> ChainPlan {
    let auth = auth_plugin(state, upstream);
    let mut guards = Vec::new();
    let mut transforms = Vec::new();
    let bindings = upstream.plugins.items.iter().chain(
        route
            .map(|route| route.plugins.items.as_slice())
            .unwrap_or_default(),
    );
    for binding in bindings {
        let guard = state.guard_plugins.get(&binding.id);
        let transform = state.transform_plugins.get(&binding.id);
        let unregistered = guard.is_none() && transform.is_none();
        if let Some(resolved_guard) = guard {
            guards.push((resolved_guard, binding.config.clone()));
        }
        if let Some(resolved_transform) = transform {
            transforms.push((resolved_transform, binding.config.clone()));
        }
        if unregistered {
            skipped_plugin(&binding.id);
        }
    }
    ChainPlan {
        auth,
        guards,
        transforms,
    }
}

/// Resolves the upstream-bound auth plugin, if any.
fn auth_plugin(state: &GearState, upstream: &Upstream) -> Option<Binding<dyn AuthPlugin>> {
    if upstream.auth.plugin_id.is_empty() {
        return None;
    }
    let plugin = state.auth_plugins.get(&upstream.auth.plugin_id);
    if plugin.is_none() {
        skipped_plugin(&upstream.auth.plugin_id);
    }
    plugin.map(|plugin| (plugin, upstream.auth.config.clone()))
}

// -------------------------------------------------------------------------------------
// Outbound request assembly
// -------------------------------------------------------------------------------------

/// Handshake headers relayed from an upstream `101` to the caller.
///
/// Only the protocol-negotiation headers are relayed: they are the ones a
/// WebSocket (or similar) client needs to validate the switch.
#[must_use]
fn upgrade_handshake(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| {
            *name == header::UPGRADE
                || *name == header::CONNECTION
                || name.as_str().starts_with("sec-websocket")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Builds the HTTP/1.1 request handed to the upstream.
fn build_outbound_request(
    method: &Method,
    upstream: &Upstream,
    endpoint: &Endpoint,
    context: RequestContext,
    _upgrade: bool,
    body: Body,
) -> Result<http::Request<Body>, DomainError> {
    let mut headers = headers_from_pairs(&context.headers);
    apply_header_rules(&mut headers, &upstream.headers.request);
    apply_passthrough_policy(&mut headers, &upstream.headers.request);
    if let Some(host) = host_value(endpoint)
        && let Ok(value) = HeaderValue::from_str(&host)
    {
        headers.insert(header::HOST, value);
    }
    let query = query_string(&context.query);
    let target = outbound_target(&context.path, &query)?;

    let mut builder = http::Request::builder().method(method.clone()).uri(target);
    for (name, value) in &headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(body)
        .map_err(|_| DomainError::new(ErrorKind::ProtocolError, "invalid outbound request"))
}

/// Outbound request path for a matched route.
#[must_use]
fn outbound_path(route: Option<&Route>, suffix: &str) -> String {
    let prefix = route
        .and_then(http_match)
        .map_or(String::new(), |rule| rule.path.clone());
    let prefix = if prefix.is_empty() || prefix.starts_with('/') {
        prefix
    } else {
        format!("/{prefix}")
    };
    if suffix.is_empty() {
        if prefix.is_empty() {
            return "/".to_owned();
        }
        return prefix;
    }
    if prefix.is_empty() {
        return suffix.to_owned();
    }
    if prefix.ends_with('/') {
        return format!("{prefix}{}", suffix.trim_start_matches('/'));
    }
    format!("{prefix}{suffix}")
}

/// Re-encodes plugin-mutated query pairs.
#[must_use]
fn query_string(pairs: &[(String, String)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

/// Origin-form request target of the upstream call: `path[?query]`.
///
/// The upstream connection is established before the request is encoded, so the
/// authority travels in the `Host` header and the socket — never in the request
/// target. RFC 9112 §3.2.2 requires origin-form when a client sends a request
/// directly to a server; emitting an absolute target would also break strict
/// upstreams that reject it.
fn outbound_target(path: &str, query: &str) -> Result<Uri, DomainError> {
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let path_and_query = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    path_and_query.parse::<Uri>().map_err(|_| {
        DomainError::new(
            ErrorKind::ProtocolError,
            "upstream target is not a valid URI",
        )
    })
}

/// `host[:port]` used both for the target URI and the `Host` header.
#[must_use]
fn endpoint_authority(endpoint: &Endpoint) -> String {
    let port = endpoint.effective_port();
    if port == endpoint.scheme.default_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{port}", endpoint.host)
    }
}

/// `host[:port]` stamped as the outbound `Host` header.
#[must_use]
fn host_value(endpoint: &Endpoint) -> Option<String> {
    Some(endpoint_authority(endpoint))
}

/// `X-OAGW-Target-Host` of the inbound request, trimmed.
#[must_use]
fn target_host(headers: &HeaderMap) -> Option<String> {
    headers
        .get(TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Declared request body length, when parseable.
#[must_use]
fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// `true` when the request framing is one we can relay.
#[must_use]
fn transfer_encoding_supported(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::TRANSFER_ENCODING) else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value
        .split(',')
        .map(str::trim)
        .all(|token| token.eq_ignore_ascii_case("chunked"))
}

/// Caps a streamed body at `max` bytes without buffering it.
///
/// Chunks are truncated at the ceiling, so the upstream never receives more
/// than `max` bytes even when the framing carries no `Content-Length`.
fn limited_body(body: Body, max: u64) -> Body {
    let mut remaining = max;
    let stream =
        futures_util::StreamExt::filter_map(body.into_data_stream(), move |chunk| match chunk {
            Err(err) => std::future::ready(Some(Err(err))),
            Ok(data) => {
                let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
                if remaining == 0 || len == 0 {
                    return std::future::ready(None);
                }
                let take = usize::try_from(remaining.min(len)).unwrap_or(0);
                remaining -= u64::try_from(take).unwrap_or(0);
                std::future::ready(Some(Ok(data.slice(..take))))
            }
        });
    Body::from_stream(stream)
}

// -------------------------------------------------------------------------------------
// Policy helpers
// -------------------------------------------------------------------------------------

/// Rejects a plaintext upstream when `allow_http_upstream` is disabled.
fn enforce_plaintext_policy(config: &OagwConfig, endpoint: &Endpoint) -> Result<(), DomainError> {
    if !endpoint.scheme.is_plaintext() || config.allow_http_upstream {
        return Ok(());
    }
    Err(DomainError::new(
        ErrorKind::Validation,
        format!(
            "plaintext upstream endpoint '{}' is refused: allow_http_upstream is disabled",
            endpoint.host
        ),
    )
    .with_extension("host", serde_json::json!(endpoint.host)))
}

/// Refuses endpoints whose host matches a denied SSRF segment.
fn enforce_ssrf_policy(config: &OagwConfig, endpoint: &Endpoint) -> Result<(), DomainError> {
    if !config.ssrf_policy.enabled {
        return Ok(());
    }
    let host = endpoint.host.to_ascii_lowercase();
    let denied = config
        .ssrf_policy
        .denied_segments
        .iter()
        .any(|segment| host_matches_segment(&host, segment));
    if denied {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("upstream endpoint '{host}' is denied by the SSRF policy"),
        )
        .with_extension("host", serde_json::json!(host)));
    }
    Ok(())
}

/// `true` when `host` is, or is under, `segment`.
///
/// Domain segments match themselves, any sub-domain and any label of the host.
/// IP literals only ever match themselves so a partial-quad segment can never
/// widen into an unrelated address.
#[must_use]
fn host_matches_segment(host: &str, segment: &str) -> bool {
    let segment = segment.trim().to_ascii_lowercase();
    if segment.is_empty() {
        return false;
    }
    if host == segment {
        return true;
    }
    if is_ip_literal(&segment) || is_ip_literal(host) {
        return false;
    }
    host.split('.').any(|label| label == segment) || host.ends_with(&format!(".{segment}"))
}

/// `true` for dotted-quad and IPv6 literals.
#[must_use]
fn is_ip_literal(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Ancestor rate limit that participates in hierarchical merging.
#[must_use]
fn enforced_limit(config: Option<&RateLimitConfig>) -> Option<RateLimitConfig> {
    config
        .filter(|limit| limit.sharing == SharingMode::Enforce)
        .cloned()
}

/// Folds ancestor, upstream and route limits into the strictest one.
#[must_use]
fn effective_rate_limit(
    upstream: Option<&RateLimitConfig>,
    ancestors: &[RateLimitConfig],
    route: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    let mut merged = None;
    for limit in ancestors {
        merged = merge_limits(merged.as_ref(), Some(limit));
    }
    merged = merge_limits(merged.as_ref(), upstream);
    merge_limits(merged.as_ref(), route)
}

/// Unions the upstream CORS block with every contributing ancestor block.
#[must_use]
fn merge_cors(upstream: &CorsConfig, ancestors: &[CorsConfig]) -> CorsConfig {
    let mut merged = upstream.clone();
    for ancestor in ancestors {
        merged.enabled |= ancestor.enabled;
        merged.allow_credentials |= ancestor.allow_credentials;
        for origin in &ancestor.allowed_origins {
            if !merged.allowed_origins.iter().any(|known| known == origin) {
                merged.allowed_origins.push(origin.clone());
            }
        }
        for name in &ancestor.allowed_methods {
            if !merged.allowed_methods.iter().any(|known| known == name) {
                merged.allowed_methods.push(name.clone());
            }
        }
        for name in &ancestor.allowed_headers {
            if !merged.allowed_headers.iter().any(|known| known == name) {
                merged.allowed_headers.push(name.clone());
            }
        }
        for name in &ancestor.expose_headers {
            if !merged.expose_headers.iter().any(|known| known == name) {
                merged.expose_headers.push(name.clone());
            }
        }
        merged.max_age = merged.max_age.max(ancestor.max_age);
    }
    merged
}

/// The HTTP matching rule of a route, when it targets HTTP.
#[must_use]
fn http_match(route: &Route) -> Option<&HttpMatch> {
    route.route_match.http.as_ref()
}

/// `true` when the route accepts the method and its prefix covers the suffix.
#[must_use]
fn route_matches(route: &Route, method: &Method, path_suffix: &str) -> bool {
    let Some(rule) = http_match(route) else {
        return false;
    };
    let allowed = rule.methods.is_empty()
        || rule
            .methods
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(method.as_str()));
    if !allowed {
        return false;
    }
    let prefix = rule.path.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    path_suffix == prefix
        || path_suffix
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/') || rest.is_empty())
}

/// Path beyond the matched route prefix, when one was supplied.
#[must_use]
fn path_remainder(route: &Route, path_suffix: &str) -> Option<String> {
    let rule = http_match(route)?;
    let prefix = rule.path.trim_end_matches('/');
    if prefix.is_empty() {
        return Some(path_suffix.to_owned());
    }
    path_suffix
        .strip_prefix(prefix)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .map(str::to_owned)
}

/// Stamps the `X-RateLimit-*` headers of an allowed request.
fn stamp_rate_headers(headers: &mut HeaderMap, rate: RateHeaders) {
    let entries = [
        ("x-ratelimit-limit", rate.limit),
        ("x-ratelimit-remaining", rate.remaining),
        ("x-ratelimit-reset", rate.reset_secs),
    ];
    for (name, value) in entries {
        if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
            headers.insert(name, value);
        }
    }
}

// -------------------------------------------------------------------------------------
// Error mapping
// -------------------------------------------------------------------------------------

/// Turns a guard rejection into a problem response.
fn ensure_allowed(decision: &GuardDecision, instance: &str) -> Result<(), DomainError> {
    let GuardDecision::Reject {
        status,
        error_code,
        message,
    } = decision
    else {
        return Ok(());
    };
    let mut failure = DomainError::new(reject_kind(*status), message.clone())
        .with_extension("error_code", serde_json::json!(error_code));
    if !instance.is_empty() {
        failure = failure.with_instance(instance.to_owned());
    }
    Err(failure)
}

/// Maps a guard's requested status onto the closest problem kind.
#[must_use]
fn reject_kind(status: u16) -> ErrorKind {
    match status {
        401 => ErrorKind::AuthenticationFailed,
        403 => ErrorKind::CorsMethodNotAllowed,
        404 => ErrorKind::RouteNotFound,
        409 => ErrorKind::Conflict,
        413 => ErrorKind::PayloadTooLarge,
        429 => ErrorKind::RateLimitExceeded,
        502 => ErrorKind::ProtocolError,
        503 => ErrorKind::LinkUnavailable,
        504 => ErrorKind::RequestTimeout,
        _ => ErrorKind::Validation,
    }
}

/// Maps a plugin failure to a gateway problem without leaking plugin detail.
#[must_use]
fn plugin_failure(err: &PluginError, kind: ErrorKind, instance: &str) -> DomainError {
    let mut failure = DomainError::new(kind, format!("plugin rejected the request: {}", err.code))
        .with_extension("error_code", serde_json::json!(err.code));
    if !instance.is_empty() {
        failure = failure.with_instance(instance.to_owned());
    }
    failure
}

/// Maps an auth plugin failure to 401.
#[must_use]
fn auth_failure(err: &PluginError) -> DomainError {
    DomainError::new(
        ErrorKind::AuthenticationFailed,
        "outbound authentication failed",
    )
    .with_extension("error_code", serde_json::json!(err.code))
}

/// Logs a binding that resolved to no registered plugin.
fn skipped_plugin(id: &str) {
    tracing::warn!(plugin = %id, "plugin binding skipped: not a registered named plugin");
}

/// 504-class problem for a timed-out phase.
#[must_use]
fn timeout_error(kind: ErrorKind, instance: &str) -> DomainError {
    DomainError::new(kind, "upstream round trip exceeded the proxy timeout")
        .with_instance(instance.to_owned())
}

/// 502 problem for a failed upstream connection.
#[must_use]
fn connect_error(instance: &str) -> DomainError {
    DomainError::new(
        ErrorKind::DownstreamError,
        "failed to connect to the upstream endpoint",
    )
    .with_instance(instance.to_owned())
}

/// 502 problem for an upstream response failure.
#[must_use]
fn upstream_error(instance: &str) -> DomainError {
    DomainError::new(ErrorKind::ProtocolError, "upstream response was malformed")
        .with_instance(instance.to_owned())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::domain::model::{EndpointScheme, ServerConfig};

    fn endpoint(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn pool(scheme: EndpointScheme, host: &str, port: Option<u16>) -> ServerConfig {
        ServerConfig {
            endpoints: vec![endpoint(scheme, host, port)],
        }
    }

    fn upstream(alias: &str, server: ServerConfig) -> Upstream {
        Upstream {
            id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            enabled: true,
            alias: alias.to_owned(),
            tags: BTreeSet::new(),
            server,
            protocol: crate::domain::model::Protocol::Http,
            auth: crate::domain::model::AuthConfig::default(),
            headers: crate::domain::model::HeadersConfig::default(),
            plugins: crate::domain::model::PluginsConfig::default(),
            rate_limit: None,
            cors: crate::domain::model::CorsConfig::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn route(upstream_id: uuid::Uuid, path: &str, methods: &[&str]) -> Route {
        Route {
            id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            upstream_id,
            enabled: true,
            tags: BTreeSet::new(),
            route_match: crate::domain::model::RouteMatch {
                http: Some(HttpMatch {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: crate::domain::model::PluginsConfig::default(),
            rate_limit: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn outbound_path_joins_prefix_and_suffix() {
        let owned = route(uuid::Uuid::nil(), "/v1/pets", &[]);
        assert_eq!(outbound_path(None, ""), "/");
        assert_eq!(outbound_path(None, "/search"), "/search");
        assert_eq!(outbound_path(Some(&owned), ""), "/v1/pets");
        assert_eq!(outbound_path(Some(&owned), "/42"), "/v1/pets/42");

        let slashed = route(uuid::Uuid::nil(), "/v1/", &[]);
        assert_eq!(outbound_path(Some(&slashed), "/42"), "/v1/42");

        let bare = route(uuid::Uuid::nil(), "v1", &[]);
        assert_eq!(outbound_path(Some(&bare), "/x"), "/v1/x");
    }

    #[test]
    fn query_string_encodes_plugin_pairs() {
        let pairs = vec![
            ("api_key".to_owned(), "Bearer s3cr3t".to_owned()),
            ("q".to_owned(), "a b&c".to_owned()),
        ];
        let encoded = query_string(&pairs);
        assert_eq!(encoded, "api_key=Bearer+s3cr3t&q=a+b%26c");
        assert_eq!(query_string(&[]), "");
    }

    #[test]
    fn outbound_target_is_always_origin_form_with_an_explicit_query() {
        assert_eq!(
            outbound_target("/v1", "")
                .map(|uri| uri.to_string())
                .unwrap_or_default(),
            "/v1"
        );
        assert_eq!(
            outbound_target("/", "")
                .map(|uri| uri.to_string())
                .unwrap_or_default(),
            "/"
        );
        assert_eq!(
            outbound_target("/v1", "a=1")
                .map(|uri| uri.to_string())
                .unwrap_or_default(),
            "/v1?a=1"
        );
        let authority = endpoint(EndpointScheme::Https, "api.example", None);
        assert_eq!(endpoint_authority(&authority), "api.example");
        let explicit = endpoint(EndpointScheme::Https, "api.example", Some(8443));
        assert_eq!(endpoint_authority(&explicit), "api.example:8443");
    }

    #[test]
    fn outbound_target_rejects_an_unformable_target() {
        // A space is never a legal request-target character.
        let err = outbound_target("/v1 bad", "").expect_err("invalid target");
        assert_eq!(err.kind, ErrorKind::ProtocolError);
    }

    #[test]
    fn plaintext_gate_follows_allow_http_upstream() {
        let allowed = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        let plain = endpoint(EndpointScheme::Http, "api.example", None);
        assert!(enforce_plaintext_policy(&allowed, &plain).is_ok());

        let locked = OagwConfig {
            allow_http_upstream: false,
            ..OagwConfig::default()
        };
        let err = enforce_plaintext_policy(&locked, &plain).expect_err("plaintext refused");
        assert_eq!(err.kind, ErrorKind::Validation);
        // TLS endpoints are never gated by the plaintext switch.
        assert!(
            enforce_plaintext_policy(
                &locked,
                &endpoint(EndpointScheme::Https, "api.example", None)
            )
            .is_ok()
        );
    }

    #[test]
    fn ssrf_policy_blocks_denied_segments_only_when_enabled() {
        let mut config = OagwConfig::default();
        config.ssrf_policy.enabled = false;
        config.ssrf_policy.denied_segments = vec!["internal".to_owned()];
        assert!(
            enforce_ssrf_policy(
                &config,
                &endpoint(EndpointScheme::Http, "internal.svc", None)
            )
            .is_ok()
        );

        config.ssrf_policy.enabled = true;
        let err = enforce_ssrf_policy(
            &config,
            &endpoint(EndpointScheme::Http, "internal.svc", None),
        )
        .expect_err("denied segment");
        assert_eq!(err.kind, ErrorKind::Validation);
        assert!(
            enforce_ssrf_policy(
                &config,
                &endpoint(EndpointScheme::Http, "api.example", None)
            )
            .is_ok()
        );
    }

    #[test]
    fn host_segment_matching_covers_suffix_and_labels() {
        assert!(host_matches_segment("internal.svc", "internal"));
        assert!(host_matches_segment("internal.svc", "internal.svc"));
        assert!(host_matches_segment("10.0.0.1", "10.0.0.1"));
        // IP segments never widen into a suffix match.
        assert!(!host_matches_segment("10.0.0.1", "0.0.0.1"));
        assert!(host_matches_segment(
            "svc.internal.example",
            "internal.example"
        ));
        assert!(host_matches_segment("internal.svc.example", "internal"));
        assert!(!host_matches_segment("notinternal.example", "internal"));
        assert!(!host_matches_segment("api.example", "internal"));
        assert!(!host_matches_segment("api.example", "example.internal"));
        assert!(!host_matches_segment("api.example", "  "));
    }

    #[test]
    fn cors_merge_unions_ancestor_blocks() {
        let upstream_cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://a.example".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            ..CorsConfig::default()
        };
        let ancestor = CorsConfig {
            enabled: false,
            allow_credentials: true,
            allowed_origins: vec![
                "https://a.example".to_owned(),
                "https://b.example".to_owned(),
            ],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            allowed_headers: vec!["X-Trace".to_owned()],
            expose_headers: vec!["X-Request-Id".to_owned()],
            max_age: Some(600),
            ..CorsConfig::default()
        };
        let merged = merge_cors(&upstream_cors, &[ancestor]);
        assert!(merged.enabled);
        assert!(merged.allow_credentials);
        assert_eq!(merged.allowed_origins.len(), 2);
        assert_eq!(merged.allowed_methods.len(), 2);
        assert_eq!(merged.allowed_headers, vec!["X-Trace".to_owned()]);
        assert_eq!(merged.expose_headers, vec!["X-Request-Id".to_owned()]);
        assert_eq!(merged.max_age, Some(600));
    }

    #[test]
    fn rate_limits_fold_to_the_strictest_side() {
        let build = |rate: u32, burst: Option<u32>| crate::domain::model::RateLimitConfig {
            sustained: crate::domain::model::SustainedRate {
                rate,
                window: crate::domain::model::RateWindow::Minute,
            },
            burst,
            ..crate::domain::model::RateLimitConfig::default()
        };
        let ancestor = build(100, Some(50));
        let route_limit = build(10, Some(5));
        let merged = effective_rate_limit(
            Some(&ancestor),
            std::slice::from_ref(&ancestor),
            Some(&route_limit),
        )
        .expect("merged limit");
        assert_eq!(merged.sustained.rate, 10);
        assert_eq!(merged.burst, Some(5));
        assert!(effective_rate_limit(None, &[], None).is_none());
        // A single source is passed through untouched.
        let only = effective_rate_limit(None, std::slice::from_ref(&ancestor), None)
            .expect("ancestor only");
        assert_eq!(only.sustained.rate, 100);
    }

    #[test]
    fn route_matching_honours_methods_and_prefixes() {
        let owned = route(uuid::Uuid::nil(), "/v1/pets", &["GET"]);
        assert!(route_matches(&owned, &Method::GET, "/v1/pets"));
        assert!(route_matches(&owned, &Method::GET, "/v1/pets/42"));
        assert!(!route_matches(&owned, &Method::GET, "/v1/petsies"));
        assert!(!route_matches(&owned, &Method::POST, "/v1/pets"));

        let any_method = route(uuid::Uuid::nil(), "", &[]);
        assert!(route_matches(&any_method, &Method::DELETE, "/anything"));
    }

    #[test]
    fn path_remainder_reports_the_unmatched_suffix() {
        let owned = route(uuid::Uuid::nil(), "/v1/pets", &[]);
        assert_eq!(path_remainder(&owned, "/v1/pets"), Some(String::new()));
        assert_eq!(
            path_remainder(&owned, "/v1/pets/42"),
            Some("/42".to_owned())
        );
        assert_eq!(path_remainder(&owned, "/v2/pets"), None);
    }

    #[test]
    fn body_framing_inspection() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("1024"));
        assert_eq!(content_length(&headers), Some(1024));
        headers.remove(header::CONTENT_LENGTH);
        assert_eq!(content_length(&headers), None);

        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        assert!(transfer_encoding_supported(&headers));
        headers.insert(header::TRANSFER_ENCODING, HeaderValue::from_static("gzip"));
        assert!(!transfer_encoding_supported(&headers));
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("gzip, chunked"),
        );
        assert!(!transfer_encoding_supported(&headers));
    }

    #[test]
    fn target_host_header_is_trimmed_and_optional() {
        let mut headers = HeaderMap::new();
        assert!(target_host(&headers).is_none());
        headers.insert(
            TARGET_HOST_HEADER,
            HeaderValue::from_static("  api.example  "),
        );
        assert_eq!(target_host(&headers).as_deref(), Some("api.example"));
        headers.insert(TARGET_HOST_HEADER, HeaderValue::from_static("   "));
        assert!(target_host(&headers).is_none());
    }

    #[test]
    fn upgrade_handshake_keeps_only_the_protocol_negotiation() {
        let mut headers = HeaderMap::new();
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(
            HeaderName::from_static("sec-websocket-accept"),
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        headers.insert(header::SERVER, HeaderValue::from_static("upstream-1"));

        let relayed = upgrade_handshake(&headers);
        let mut names: Vec<String> = relayed
            .iter()
            .map(|(name, _)| name.as_str().to_owned())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["connection", "sec-websocket-accept", "upgrade"],
            "only the protocol negotiation headers are relayed"
        );
    }

    #[test]
    fn rate_headers_are_stamped_as_a_triple() {
        let mut headers = HeaderMap::new();
        stamp_rate_headers(
            &mut headers,
            RateHeaders {
                limit: 10,
                remaining: 3,
                reset_secs: 7,
            },
        );
        assert_eq!(
            headers
                .get("x-ratelimit-limit")
                .and_then(|v| v.to_str().ok()),
            Some("10")
        );
        assert_eq!(
            headers
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok()),
            Some("3")
        );
        assert_eq!(
            headers
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );
    }

    #[test]
    fn guard_status_maps_onto_the_closest_problem_kind() {
        assert_eq!(reject_kind(401), ErrorKind::AuthenticationFailed);
        assert_eq!(reject_kind(429), ErrorKind::RateLimitExceeded);
        assert_eq!(reject_kind(503), ErrorKind::LinkUnavailable);
        assert_eq!(reject_kind(418), ErrorKind::Validation);
    }

    #[test]
    fn outbound_request_rebuilds_host_and_drops_hop_by_hop() {
        let mut server = pool(EndpointScheme::Http, "api.example", Some(8080));
        server
            .endpoints
            .push(endpoint(EndpointScheme::Http, "api.example", Some(8080)));
        let owned = upstream("api.example", server);

        let request = http::Request::builder()
            .method(Method::GET)
            .uri("http://gateway/oagw/v1/proxy/api.example/v1?marker=1")
            .header(header::HOST, "gateway")
            .header(header::CONNECTION, "keep-alive")
            .header("x-oagw-target-host", "api.example")
            .header("accept", "application/json")
            .body(Body::empty())
            .expect("well formed inbound request");

        let security_context = toolkit_security::SecurityContext::anonymous();
        let proxy_request = ProxyRequest {
            alias: "api.example".to_owned(),
            path_suffix: "/v1".to_owned(),
            request,
            client_ip: "127.0.0.1".to_owned(),
            security_context: Some(security_context.clone()),
        };
        let context = build_request_context(&security_context, &owned, 0, &proxy_request, None);
        let outbound = build_outbound_request(
            &Method::GET,
            &owned,
            &owned.server.endpoints[0],
            context,
            false,
            Body::empty(),
        )
        .expect("outbound request assembles")
        .into_parts()
        .0;

        // Origin-form target: the authority lives in `Host`, never in the URI.
        assert_eq!(outbound.uri.to_string(), "/v1?marker=1");
        assert_eq!(
            outbound
                .headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok()),
            Some("api.example:8080")
        );
        assert!(outbound.headers.get(header::CONNECTION).is_none());
        assert!(outbound.headers.get("x-oagw-target-host").is_none());
        assert_eq!(
            outbound
                .headers
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn limited_body_stops_at_the_ceiling() {
        let payload = vec![7_u8; 4096];
        let body = Body::from(payload.clone());
        let capped = limited_body(body, 16);
        let bytes = axum::body::to_bytes(capped, 4096).await.unwrap_or_default();
        assert_eq!(bytes.len(), 16);

        let body = Body::from(payload);
        let bytes = axum::body::to_bytes(limited_body(body, u64::MAX), 8192)
            .await
            .unwrap_or_default();
        assert_eq!(bytes.len(), 4096);
    }

    #[test]
    fn single_endpoint_pools_ignore_the_target_host_header() {
        let single = upstream(
            "api.example",
            pool(EndpointScheme::Https, "api.example", None),
        );
        assert_eq!(
            resolve_target_host(&single, Some("other.example"))
                .map(|endpoint| endpoint.host.as_str())
                .unwrap_or_default(),
            "api.example"
        );
    }

    /// A two-host pool whose alias is the hosts' common suffix.
    fn regional_pool() -> Upstream {
        let server = ServerConfig {
            endpoints: vec![
                endpoint(EndpointScheme::Https, "us.vendor.example", None),
                endpoint(EndpointScheme::Https, "eu.vendor.example", None),
            ],
        };
        upstream("vendor.example", server)
    }

    #[test]
    fn common_suffix_alias_requires_a_pinned_target_host() {
        let owned = regional_pool();

        let missing = resolve_target_host(&owned, None).expect_err("header required");
        assert_eq!(missing.kind, ErrorKind::MissingTargetHost);
        assert!(
            missing
                .extensions
                .iter()
                .any(|(key, value)| key == "valid_hosts"
                    && value == &serde_json::json!(["eu.vendor.example", "us.vendor.example"])),
            "the problem lists the valid hosts: {:?}",
            missing.extensions
        );

        let unknown =
            resolve_target_host(&owned, Some("ap.vendor.example")).expect_err("not a pool member");
        assert_eq!(unknown.kind, ErrorKind::UnknownTargetHost);

        let with_port = resolve_target_host(&owned, Some("us.vendor.example:443"))
            .expect_err("ports are not hostnames");
        assert_eq!(with_port.kind, ErrorKind::InvalidTargetHost);

        let pinned = resolve_target_host(&owned, Some("EU.VENDOR.EXAMPLE"))
            .expect("a pool member pins the endpoint");
        assert_eq!(pinned.host, "eu.vendor.example");
    }

    #[test]
    fn explicit_aliases_route_without_a_target_host() {
        let server = ServerConfig {
            endpoints: vec![
                endpoint(EndpointScheme::Https, "api.vendor.example", None),
                endpoint(EndpointScheme::Https, "cdn.other.example", None),
            ],
        };
        let owned = upstream("fleet", server);
        let selected = resolve_target_host(&owned, None).expect("no header needed");
        assert_eq!(selected.host, "api.vendor.example");
    }

    #[test]
    fn duplicated_hosts_do_not_require_disambiguation() {
        let server = ServerConfig {
            endpoints: vec![
                endpoint(EndpointScheme::Https, "api.example", Some(443)),
                endpoint(EndpointScheme::Https, "api.example", Some(8443)),
            ],
        };
        let owned = upstream("api.example", server);
        let selected = resolve_target_host(&owned, None).expect("redundant pool");
        assert_eq!(selected.port, Some(443));
    }
}
